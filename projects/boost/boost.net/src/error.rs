//! Errors shared by every `boost_net` protocol.

use std::{
    error::Error as StdError,
    fmt::{Display, Formatter},
    io,
};

/// Errors produced while constructing or running a network request.
#[derive(Debug)]
pub enum NetError {
    /// The selected transport cannot handle the request.
    Unsupported(String),
    /// The connection failed before a response was produced.
    Transport(String),
    /// The peer violated the protocol contract.
    Protocol(String),
    /// Reading a response body failed.
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
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(message) => write!(formatter, "unsupported request: {message}"),
            Self::Transport(message) => write!(formatter, "transport error: {message}"),
            Self::Protocol(message) => write!(formatter, "protocol error: {message}"),
            Self::Body(error) => write!(formatter, "response body error: {error}"),
            Self::BodyTooLarge { limit } => write!(formatter, "response body exceeds {limit} bytes"),
            Self::HeaderTooLarge { limit } => write!(formatter, "response headers exceed {limit} bytes"),
            Self::Closed => formatter.write_str("connection is closed"),
        }
    }
}

impl StdError for NetError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Body(error) => Some(error),
            _ => None,
        }
    }
}
