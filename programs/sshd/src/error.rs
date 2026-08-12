//! What ends a connection, and how it reads in the log.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// The socket failed or the peer went away mid-message.
    Io(&'static str),
    /// The peer sent something the protocol does not allow at this point.
    Protocol(String),
    /// The peer sent `SSH_MSG_DISCONNECT`, which is an orderly end.
    PeerDisconnect(String),
    /// Authentication never succeeded.
    AuthFailed,
    /// A local resource the session needs could not be had.
    Local(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(what) => write!(f, "connection lost: {what}"),
            Self::Protocol(why) => write!(f, "protocol error: {why}"),
            Self::PeerDisconnect(why) => write!(f, "peer disconnected: {why}"),
            Self::AuthFailed => write!(f, "authentication failed"),
            Self::Local(why) => write!(f, "{why}"),
        }
    }
}

/// Shorthand for the common `Err(Protocol(format!(...)))`.
macro_rules! protocol_err {
    ($($arg:tt)*) => {
        Err($crate::error::Error::Protocol(format!($($arg)*)))
    };
}

pub(crate) use protocol_err;
