//! UNIX related logic for terminal manipulation.

use crate::event::filter::CursorPositionFilter;
use crate::event::read::InternalEventReader;
use crate::event::source::unix::WinchSignalReceiver;
use crate::event::{poll_internal2, read_internal2, EventStream, InternalEvent};
use crate::terminal::WindowSize;
#[cfg(feature = "libc")]
use libc::{
    cfmakeraw, ioctl, tcgetattr, tcsetattr, termios as Termios, winsize, STDOUT_FILENO, TCSANOW,
    TIOCGWINSZ,
};
use parking_lot::{MappedMutexGuard, Mutex, MutexGuard};
#[cfg(not(feature = "libc"))]
use rustix::{
    fd::AsFd,
    termios::{Termios, Winsize},
};

use std::{
    fs::File,
    io,
    io::{BufWriter, Error, ErrorKind, Write},
    process,
    sync::Arc,
    time::Duration,
};
#[cfg(feature = "libc")]
use std::{
    mem,
    os::unix::io::{IntoRawFd, RawFd},
};

pub struct Terminal {
    file: BufWriter<File>,
    prior_mode: Option<Termios>,
    event_stream: Option<EventStream>,
    event_reader: Option<Arc<Mutex<InternalEventReader>>>,
}

impl Write for Terminal {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

static TERMINAL: Mutex<Option<Terminal>> = parking_lot::const_mutex(None);

pub(crate) fn terminal<'a>() -> io::Result<MappedMutexGuard<'a, Terminal>> {
    let mut terminal = TERMINAL.lock();
    if terminal.is_none() {
        *terminal = Some(Terminal {
            file: BufWriter::new(File::options().read(true).write(true).open("/dev/tty")?),
            prior_mode: None,
            event_stream: None,
            event_reader: None,
        });
    }
    Ok(MutexGuard::map(terminal, |t| t.as_mut().unwrap()))
}

// Some(Termios) -> we're in the raw mode and this is the previous mode
// None -> we're not in the raw mode
static TERMINAL_MODE_PRIOR_RAW_MODE: Mutex<Option<Termios>> = parking_lot::const_mutex(None);

impl Terminal {
    pub fn new(file: File, winch_signal_receiver: WinchSignalReceiver) -> Terminal {
        let reader = Arc::new(Mutex::new(InternalEventReader::with_unix_term(
            file.try_clone().unwrap(),
            winch_signal_receiver,
        )));
        Terminal {
            file: BufWriter::new(file),
            prior_mode: None,
            event_stream: Some(EventStream::with_event_reader(reader.clone())),
            event_reader: Some(reader),
        }
    }

    pub fn is_raw_mode_enabled(&self) -> bool {
        self.prior_mode.is_some()
    }
}

pub(crate) fn is_raw_mode_enabled() -> bool {
    TERMINAL
        .lock()
        .as_ref()
        .is_some_and(|t| t.is_raw_mode_enabled())
}

#[cfg(feature = "libc")]
impl From<winsize> for WindowSize {
    fn from(size: winsize) -> WindowSize {
        WindowSize {
            columns: size.ws_col,
            rows: size.ws_row,
            width: size.ws_xpixel,
            height: size.ws_ypixel,
        }
    }
}
#[cfg(not(feature = "libc"))]
impl From<Winsize> for WindowSize {
    fn from(size: Winsize) -> WindowSize {
        WindowSize {
            columns: size.ws_col,
            rows: size.ws_row,
            width: size.ws_xpixel,
            height: size.ws_ypixel,
        }
    }
}

#[allow(clippy::useless_conversion)]
#[cfg(feature = "libc")]
pub(crate) fn window_size() -> io::Result<WindowSize> {
    // http://rosettacode.org/wiki/Terminal_control/Dimensions#Library:_BSD_libc
    let mut size = winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let file = File::open("/dev/tty").map(|file| (FileDesc::new(file.into_raw_fd(), true)));
    let fd = if let Ok(file) = &file {
        file.raw_fd()
    } else {
        // Fallback to libc::STDOUT_FILENO if /dev/tty is missing
        STDOUT_FILENO
    };

    if wrap_with_result(unsafe { ioctl(fd, TIOCGWINSZ.into(), &mut size) }).is_ok() {
        return Ok(size.into());
    }

    Err(std::io::Error::last_os_error().into())
}

#[cfg(not(feature = "libc"))]
pub(crate) fn window_size() -> io::Result<WindowSize> {
    terminal()?.window_size()
}

impl Terminal {
    pub fn window_size(&self) -> io::Result<WindowSize> {
        let size = rustix::termios::tcgetwinsize(&self.file.get_ref())?;
        Ok(size.into())
    }
}

pub(crate) fn size() -> io::Result<(u16, u16)> {
    terminal()?.size()
}

impl Terminal {
    #[allow(clippy::useless_conversion)]
    pub fn size(&self) -> io::Result<(u16, u16)> {
        if let Ok(window_size) = self.window_size() {
            return Ok((window_size.columns, window_size.rows));
        }

        tput_size().ok_or_else(|| std::io::Error::last_os_error().into())
    }

    pub fn input_stream(&mut self) -> &mut EventStream {
        self.event_stream.as_mut().unwrap()
    }

    pub fn take_input_stream(&mut self) -> EventStream {
        self.event_stream.take().unwrap()
    }
}

#[cfg(feature = "libc")]
pub(crate) fn enable_raw_mode() -> io::Result<()> {
    let mut original_mode = TERMINAL_MODE_PRIOR_RAW_MODE.lock();
    if original_mode.is_some() {
        return Ok(());
    }

    let tty = tty_fd()?;
    let fd = tty.raw_fd();
    let mut ios = get_terminal_attr(fd)?;
    let original_mode_ios = ios;
    raw_terminal_attr(&mut ios);
    set_terminal_attr(fd, &ios)?;
    // Keep it last - set the original mode only if we were able to switch to the raw mode
    *original_mode = Some(original_mode_ios);
    Ok(())
}

#[cfg(not(feature = "libc"))]
pub(crate) fn enable_raw_mode() -> io::Result<()> {
    terminal()?.enable_raw_mode()
}

impl Terminal {
    pub fn enable_raw_mode(&mut self) -> io::Result<()> {
        if self.prior_mode.is_some() {
            return Ok(());
        }

        let mut ios = get_terminal_attr(&self.file.get_ref().as_fd())?;
        let original_mode_ios = ios.clone();
        ios.make_raw();
        set_terminal_attr(&self.file.get_ref().as_fd(), &ios)?;
        // Keep it last - set the original mode only if we were able to switch to the raw mode
        self.prior_mode = Some(original_mode_ios);
        Ok(())
    }
}

/// Reset the raw mode.
///
/// More precisely, reset the whole termios mode to what it was before the first call
/// to [enable_raw_mode]. If you don't mess with termios outside of crossterm, it's
/// effectively disabling the raw mode and doing nothing else.
#[cfg(feature = "libc")]
pub(crate) fn disable_raw_mode() -> io::Result<()> {
    let mut original_mode = TERMINAL_MODE_PRIOR_RAW_MODE.lock();
    if let Some(original_mode_ios) = original_mode.as_ref() {
        let tty = tty_fd()?;
        set_terminal_attr(tty.raw_fd(), original_mode_ios)?;
        // Keep it last - remove the original mode only if we were able to switch back
        *original_mode = None;
    }
    Ok(())
}

#[cfg(not(feature = "libc"))]
pub(crate) fn disable_raw_mode() -> io::Result<()> {
    terminal()?.disable_raw_mode()
}

impl Terminal {
    pub fn disable_raw_mode(&mut self) -> io::Result<()> {
        if let Some(original_mode_ios) = self.prior_mode.as_ref() {
            set_terminal_attr(&self.file.get_ref().as_fd(), original_mode_ios)?;
            // Keep it last - remove the original mode only if we were able to switch back
            self.prior_mode = None;
        }
        Ok(())
    }
}

#[cfg(not(feature = "libc"))]
fn get_terminal_attr(fd: impl AsFd) -> io::Result<Termios> {
    let result = rustix::termios::tcgetattr(fd)?;
    Ok(result)
}

#[cfg(not(feature = "libc"))]
fn set_terminal_attr(fd: impl AsFd, termios: &Termios) -> io::Result<()> {
    rustix::termios::tcsetattr(fd, rustix::termios::OptionalActions::Now, termios)?;
    Ok(())
}

/// Queries the terminal's support for progressive keyboard enhancement.
///
/// On unix systems, this function will block and possibly time out while
/// [`crossterm::event::read`](crate::event::read) or [`crossterm::event::poll`](crate::event::poll) are being called.
#[cfg(feature = "events")]
pub fn supports_keyboard_enhancement() -> io::Result<bool> {
    terminal()?.supports_keyboard_enhancement()
}

impl Terminal {
    pub fn supports_keyboard_enhancement(&mut self) -> io::Result<bool> {
        if self.is_raw_mode_enabled() {
            read_supports_keyboard_enhancement_raw(self)
        } else {
            read_supports_keyboard_enhancement_flags(self)
        }
    }
}

#[cfg(feature = "events")]
fn read_supports_keyboard_enhancement_flags(terminal: &mut Terminal) -> io::Result<bool> {
    terminal.enable_raw_mode()?;
    let flags = read_supports_keyboard_enhancement_raw(terminal);
    terminal.disable_raw_mode()?;
    flags
}

#[cfg(feature = "events")]
fn read_supports_keyboard_enhancement_raw(terminal: &mut Terminal) -> io::Result<bool> {
    use crate::event::{
        filter::{KeyboardEnhancementFlagsFilter, PrimaryDeviceAttributesFilter},
        poll_internal2, read_internal2, InternalEvent,
    };
    use std::io::Write;
    use std::time::Duration;

    // This is the recommended method for testing support for the keyboard enhancement protocol.
    // We send a query for the flags supported by the terminal and then the primary device attributes
    // query. If we receive the primary device attributes response but not the keyboard enhancement
    // flags, none of the flags are supported.
    //
    // See <https://sw.kovidgoyal.net/kitty/keyboard-protocol/#detection-of-support-for-this-protocol>

    // ESC [ ? u        Query progressive keyboard enhancement flags (kitty protocol).
    // ESC [ c          Query primary device attributes.
    const QUERY: &[u8] = b"\x1B[?u\x1B[c";

    terminal.file.write_all(QUERY)?;
    terminal.file.flush()?;

    let reader = terminal.event_reader.as_mut().unwrap();
    loop {
        match poll_internal2(
            reader,
            Some(Duration::from_millis(2000)),
            &KeyboardEnhancementFlagsFilter,
        ) {
            Ok(true) => {
                match read_internal2(reader, &KeyboardEnhancementFlagsFilter) {
                    Ok(InternalEvent::KeyboardEnhancementFlags(_current_flags)) => {
                        // Flush the PrimaryDeviceAttributes out of the event queue.
                        read_internal2(reader, &PrimaryDeviceAttributesFilter).ok();
                        return Ok(true);
                    }
                    _ => return Ok(false),
                }
            }
            Ok(false) => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "The keyboard enhancement status could not be read within a normal duration",
                ));
            }
            Err(_) => {}
        }
    }
}

/// execute tput with the given argument and parse
/// the output as a u16.
///
/// The arg should be "cols" or "lines"
fn tput_value(arg: &str) -> Option<u16> {
    let output = process::Command::new("tput").arg(arg).output().ok()?;
    let value = output
        .stdout
        .into_iter()
        .filter_map(|b| char::from(b).to_digit(10))
        .fold(0, |v, n| v * 10 + n as u16);

    if value > 0 {
        Some(value)
    } else {
        None
    }
}

/// Returns the size of the screen as determined by tput.
///
/// This alternate way of computing the size is useful
/// when in a subshell.
fn tput_size() -> Option<(u16, u16)> {
    match (tput_value("cols"), tput_value("lines")) {
        (Some(w), Some(h)) => Some((w, h)),
        _ => None,
    }
}

#[cfg(feature = "libc")]
// Transform the given mode into an raw mode (non-canonical) mode.
fn raw_terminal_attr(termios: &mut Termios) {
    unsafe { cfmakeraw(termios) }
}

#[cfg(feature = "libc")]
fn get_terminal_attr(fd: RawFd) -> io::Result<Termios> {
    unsafe {
        let mut termios = mem::zeroed();
        wrap_with_result(tcgetattr(fd, &mut termios))?;
        Ok(termios)
    }
}

#[cfg(feature = "libc")]
fn set_terminal_attr(fd: RawFd, termios: &Termios) -> io::Result<()> {
    wrap_with_result(unsafe { tcsetattr(fd, TCSANOW, termios) })
}

#[cfg(feature = "libc")]
fn wrap_with_result(result: i32) -> io::Result<()> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl Terminal {
    pub fn cursor_position(&mut self) -> io::Result<(u16, u16)> {
        if is_raw_mode_enabled() {
            self.read_position_raw()
        } else {
            self.read_position()
        }
    }

    fn read_position(&mut self) -> io::Result<(u16, u16)> {
        self.enable_raw_mode()?;
        let pos = self.read_position_raw();
        self.disable_raw_mode()?;
        pos
    }

    fn read_position_raw(&mut self) -> io::Result<(u16, u16)> {
        // Use `ESC [ 6 n` to and retrieve the cursor position.
        let mut stdout = io::stdout();
        stdout.write_all(b"\x1B[6n")?;
        stdout.flush()?;

        let reader = self.event_reader.as_mut().unwrap();
        loop {
            match poll_internal2(
                reader,
                Some(Duration::from_millis(2000)),
                &CursorPositionFilter,
            ) {
                Ok(true) => {
                    if let Ok(InternalEvent::CursorPosition(x, y)) =
                        read_internal2(reader, &CursorPositionFilter)
                    {
                        return Ok((x, y));
                    }
                }
                Ok(false) => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        "The cursor position could not be read within a normal duration",
                    ));
                }
                Err(_) => {}
            }
        }
    }
}
