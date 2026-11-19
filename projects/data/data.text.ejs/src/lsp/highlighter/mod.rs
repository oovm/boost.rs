#![doc = include_str!("readme.md")]

use data_text_core::errors::ParseResult;
use language_highlight::syntax::{HighlightSource, HighlightResult};
use language_highlight::themes::Theme;
pub struct JavaScriptHighlighter;

impl HighlightSource for JavaScriptHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, _theme: Theme) -> ParseResult<HighlightResult<'a>> {
        Ok(HighlightResult { segments: Vec::new(), source: source.into() })
    }
}
