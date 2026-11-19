#![doc = include_str!("readme.md")]

use data_text_core::errors::ParseResult;
use language_highlight::syntax::{HighlightSource, HighlightResult};
use language_highlight::themes::Theme;
/// Javadoc highlighter implementation.
pub struct JavadocHighlighter;

impl HighlightSource for JavadocHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, _theme: Theme) -> ParseResult<HighlightResult<'a>> {
        // FIXME: Implement real highlighting
        Ok(HighlightResult { segments: vec![], source: std::borrow::Cow::Borrowed(source) })
    }
}
