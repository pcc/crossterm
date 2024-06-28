#[cfg(feature = "use-dev-tty")]
pub(crate) mod tty;

#[cfg(not(feature = "use-dev-tty"))]
pub(crate) mod mio;

#[cfg(feature = "use-dev-tty")]
pub(crate) use self::tty::UnixInternalEventSource;
#[cfg(feature = "use-dev-tty")]
pub use self::tty::{WinchSignalReceiver, winch_signal_receiver};

#[cfg(not(feature = "use-dev-tty"))]
pub(crate) use self::mio::UnixInternalEventSource;
#[cfg(not(feature = "use-dev-tty"))]
pub use self::mio::{WinchSignalReceiver, winch_signal_receiver};
