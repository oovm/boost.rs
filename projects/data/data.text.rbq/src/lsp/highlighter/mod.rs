#![doc = include_str!("readme.md")]
use language_highlight::{
    syntax::{HighlightResult, HighlightSource},
    themes::Theme,
};
use std::borrow::Cow;

/// RBQ highlighter.
pub struct RbqHighlighter;

impl HighlightSource for RbqHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, _theme: Theme) -> data_text_core::errors::ParseResult<HighlightResult<'a>> {
        Ok(HighlightResult { segments: Vec::new(), source: Cow::Borrowed(source) })
    }
}
