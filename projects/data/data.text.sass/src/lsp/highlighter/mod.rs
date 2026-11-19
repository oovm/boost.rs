#![doc = include_str!("readme.md")]

use data_text_core::errors::ParseResult;
use language_highlight::syntax::{HighlightSource, HighlightResult};
use language_highlight::themes::Theme;
/// Sass highlighter
pub struct SassHighlighter {}

impl SassHighlighter {
    pub fn new() -> Self {
        Self {}
    }
}

impl HighlightSource for SassHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, _theme: Theme) -> ParseResult<HighlightResult<'a>> {
        // Temporarily return a simple result
        Ok(HighlightResult { segments: vec![], source: std::borrow::Cow::Borrowed(source) })
    }
}
