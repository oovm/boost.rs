//! Stable failure surface of the container owner.
//!
//! Every operation returns [`Result`], and every failure carries a code that
//! callers compare instead of parsing prose. The code stays the same when the
//! message is reworded or translated. When the parser knows which bytes it
//! rejected, the failure also carries an explicit [`Span`].

use std::fmt;

use serde::{Deserialize, Serialize};

/// Result of one container operation.
pub type Result<T> = std::result::Result<T, ContainerError>;

/// A half-open byte range `start..end` inside the container image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Span {
    /// First byte of the range.
    pub start: u64,
    /// One past the last byte of the range.
    pub end: u64,
}

impl Span {
    /// Builds a span from its bounds.
    pub fn new(start: u64, end: u64) -> Self {
        Self { start, end }
    }

    /// Number of bytes the span covers.
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// Whether the span covers no bytes.
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// Stable failure with a comparable code, a message and an optional span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerError {
    code: &'static str,
    message: String,
    span: Option<Span>,
}

impl ContainerError {
    /// Builds a failure from a stable code and a message.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), span: None }
    }

    /// A recognised container could not be decoded.
    pub fn parse(message: impl Into<String>) -> Self {
        Self::new("binary/parse", message)
    }

    /// An offset, length or handle falls outside what the image provides.
    pub fn range(message: impl Into<String>) -> Self {
        Self::new("binary/range", message)
    }

    /// The container or feature is not supported by this build.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new("binary/unsupported", message)
    }

    /// The file could not be read or written.
    pub fn io(message: impl Into<String>) -> Self {
        Self::new("binary/io", message)
    }

    /// A declared length is over the byte budget this decoder allows.
    pub fn budget(message: impl Into<String>) -> Self {
        Self::new("binary/budget", message)
    }

    /// Attaches the rejected byte range.
    pub fn with_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }

    /// Attaches the rejected byte range from its bounds.
    pub fn at(self, start: u64, end: u64) -> Self {
        self.with_span(Span::new(start, end))
    }

    /// The stable code, such as `binary/parse`.
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The human-readable message without the code prefix.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The rejected byte range, when the parser knows it.
    pub fn span(&self) -> Option<Span> {
        self.span
    }
}

impl fmt::Display for ContainerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)?;
        if let Some(span) = self.span {
            write!(formatter, " (at {}..{})", span.start, span.end)?;
        }
        Ok(())
    }
}

impl std::error::Error for ContainerError {}

impl From<std::io::Error> for ContainerError {
    fn from(error: std::io::Error) -> Self {
        Self::io(error.to_string())
    }
}
