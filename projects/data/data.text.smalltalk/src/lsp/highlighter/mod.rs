#![doc = include_str!("readme.md")]

use data_text_core::errors::ParseResult;
use language_highlight::syntax::{HighlightSource, HighlightResult};
use language_highlight::themes::Theme;
/// Smalltalk syntax highlighter
pub struct SmalltalkHighlighter;

impl SmalltalkHighlighter {
    /// Creates a new Smalltalk highlighter instance
    pub fn new() -> Self {
        Self
    }
}

impl Default for SmalltalkHighlighter {
    fn default() -> Self {
        Self::new()
    }
}

impl HighlightSource for SmalltalkHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, _theme: Theme) -> ParseResult<HighlightResult<'a>> {
        // TODO: Implement real Smalltalk highlighting logic
        Ok(HighlightResult { segments: Vec::new(), source: std::borrow::Cow::Borrowed(source) })
    }
}
