#![doc = include_str!("readme.md")]

use data_text_core::errors::ParseResult;
use language_highlight::{
    syntax::{HighlightResult, HighlightSource},
    themes::Theme,
};
/// Typst syntax highlighter
pub struct TypstHighlighter;

impl HighlightSource for TypstHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, _theme: Theme) -> ParseResult<HighlightResult<'a>> {
        Ok(HighlightResult { segments: Vec::new(), source: std::borrow::Cow::Borrowed(source) })
    }
}
