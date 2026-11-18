use std::fmt::{Display, Formatter};

/// An error emitted by the VON lexer or parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VonParseError {
    message: String,
    position: usize,
}

impl VonParseError {
    pub(crate) fn new(position: usize, message: impl Into<String>) -> Self {
        Self { message: message.into(), position }
    }
}

impl Display for VonParseError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "VON parse error at {}: {}", self.position, self.message)
    }
}

impl std::error::Error for VonParseError {}
