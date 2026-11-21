use std::fmt;

use serde::Serialize;

/// A failure reported by the PDF engine.
///
/// The code is part of the public contract and stays stable across message
/// changes, so callers compare codes instead of parsing text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PdfError {
    code: String,
    message: String,
}

impl PdfError {
    /// Builds an error from a stable code and a human-readable message.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }

    /// Returns the stable error code.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Returns the human-readable message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for PdfError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for PdfError {}

/// Result alias used by every fallible engine entry point.
pub type Result<T> = std::result::Result<T, PdfError>;

pub fn argument(message: impl Into<String>) -> PdfError {
    PdfError::new("session/argument-invalid", message)
}

pub fn invalid(message: impl Into<String>) -> PdfError {
    PdfError::new("format/invalid", message)
}

pub fn unsupported(message: impl Into<String>) -> PdfError {
    PdfError::new("format/unsupported", message)
}

pub fn read_error(message: impl Into<String>) -> PdfError {
    PdfError::new("io/read", message)
}

pub fn write_error(message: impl Into<String>) -> PdfError {
    PdfError::new("io/write", message)
}

pub fn exists_error(message: impl Into<String>) -> PdfError {
    PdfError::new("io/exists", message)
}
