//! ACME-specific failure classification.

use core::fmt;

/// Why an ACME operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcmeError {
    /// The peer answered with a document or status that violates RFC 8555.
    Protocol(String),
    /// The exchange could not complete over the network.
    Network(String),
    /// The server refused the request (an authorization or policy failure).
    Denied(String),
    /// The caller supplied an unusable argument.
    InvalidArgument(String),
}

impl AcmeError {
    /// Build a protocol error.
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(message.into())
    }

    /// Build a network error.
    pub fn network(message: impl Into<String>) -> Self {
        Self::Network(message.into())
    }

    /// Build a denial error.
    pub fn denied(message: impl Into<String>) -> Self {
        Self::Denied(message.into())
    }

    /// Build an invalid-argument error.
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::InvalidArgument(message.into())
    }

    /// The human-readable detail, without the class prefix.
    pub fn message(&self) -> &str {
        match self {
            Self::Protocol(message) | Self::Network(message) | Self::Denied(message) | Self::InvalidArgument(message) => message,
        }
    }
}

impl fmt::Display for AcmeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let class = match self {
            Self::Protocol(_) => "protocol",
            Self::Network(_) => "network",
            Self::Denied(_) => "denied",
            Self::InvalidArgument(_) => "invalid argument",
        };
        write!(formatter, "acme {class} error: {}", self.message())
    }
}

impl std::error::Error for AcmeError {}

/// Result alias for ACME operations.
pub type AcmeResult<T> = core::result::Result<T, AcmeError>;
