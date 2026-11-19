//! Errors shared by every `boost_net` protocol.

use alloc::string::String;
use core::fmt::{Display, Formatter};
#[cfg(feature = "std")]
use std::io;

/// Errors produced while constructing or running a network request.
#[derive(Debug)]
pub enum NetError {
    /// The selected transport cannot handle the request.
    Unsupported(String),
    /// The connection failed before a response was produced.
    Transport(String),
    /// The peer violated the protocol contract.
    Protocol(String),
    /// The opening handshake was answered with a status instead of `101`.
    Rejected {
        /// HTTP status of the rejection response.
        status: u16,
        /// Value of the `Date` response header, when the peer sent one.
        date: Option<String>,
    },
    /// Reading a response body failed.
    #[cfg(feature = "std")]
    Body(io::Error),
    /// The response body or message exceeded the configured limit.
    BodyTooLarge {
        /// Maximum permitted body size.
        limit: usize,
    },
    /// The response headers exceeded the configured limit.
    HeaderTooLarge {
        /// Maximum permitted header block size.
        limit: usize,
    },
    /// The connection or session was already closed.
    Closed,
}

impl Display for NetError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unsupported(message) => write!(formatter, "unsupported request: {message}"),
            Self::Transport(message) => write!(formatter, "transport error: {message}"),
            Self::Protocol(message) => write!(formatter, "protocol error: {message}"),
            Self::Rejected { status, .. } => write!(formatter, "handshake rejected with status {status}"),
            #[cfg(feature = "std")]
            Self::Body(error) => write!(formatter, "response body error: {error}"),
            Self::BodyTooLarge { limit } => write!(formatter, "response body exceeds {limit} bytes"),
            Self::HeaderTooLarge { limit } => write!(formatter, "response headers exceed {limit} bytes"),
            Self::Closed => formatter.write_str("connection is closed"),
        }
    }
}

impl core::error::Error for NetError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            #[cfg(feature = "std")]
            Self::Body(error) => Some(error),
            _ => None,
        }
    }
}
