#![doc = include_str!("readme.md")]

use language_highlight::syntax::{HighlightSource, HighlightResult};
use language_highlight::themes::Theme;
pub struct LeanHighlighter;

impl HighlightSource for LeanHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, _theme: Theme) -> data_text_core::errors::ParseResult<HighlightResult<'a>> {
        // TODO: Implement actual highlighting
        Ok(HighlightResult { segments: Vec::new(), source: std::borrow::Cow::Borrowed(source) })
    }
}
