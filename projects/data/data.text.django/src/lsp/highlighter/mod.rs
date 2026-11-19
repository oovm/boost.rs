#![doc = include_str!("readme.md")]

use data_text_core::errors::ParseResult;
use language_highlight::{
    syntax::{HighlightResult, HighlightSource, TreeHighlighter},
    themes::Theme,
};
/// Django syntax highlighter.
///
/// This module provides syntax highlighting for Django source code, supporting keywords, tags, filters, comments, etc.
pub struct DjangoHighlighter;

impl DjangoHighlighter {
    pub fn new() -> Self {
        Self
    }
}

impl HighlightSource for DjangoHighlighter {
    fn highlight<'a>(&self, source: &'a str, language: &str, theme: Theme) -> ParseResult<HighlightResult<'a>> {
        let highlighter = TreeHighlighter::new();
        highlighter.highlight(source, language, theme)
    }
}
