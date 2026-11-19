//! Errors and the crate `Result` alias.

use std::fmt;

/// Host executable assembly error carrying a diagnostic message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    message: String,
}

impl Error {
    /// Builds an error from a diagnostic message.
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// Crate `Result` alias.
pub type Result<T> = std::result::Result<T, Error>;
