#![doc = include_str!("readme.md")]
use data_text_core::errors::ParseResult;
use language_highlight::{
    syntax::{HighlightResult, HighlightSource},
    themes::Theme,
};
use std::borrow::Cow;

/// Scala highlighter.
pub struct ScalaHighlighter {
    _use_parser: bool,
}

impl HighlightSource for ScalaHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, _theme: Theme) -> ParseResult<HighlightResult<'a>> {
        Ok(HighlightResult { segments: Vec::new(), source: Cow::Borrowed(source) })
    }
}
