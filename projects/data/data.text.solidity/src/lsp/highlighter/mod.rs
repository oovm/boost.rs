#![doc = include_str!("readme.md")]
use crate::language::SolidityLanguage;
use data_text_core::{Language, errors::ParseResult};
use language_highlight::{
    syntax::{HighlightResult, HighlightSource, TreeHighlighter},
    themes::Theme,
};

/// Solidity syntax highlighter.
pub struct SolidityHighlighter;

impl HighlightSource for SolidityHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, theme: Theme) -> ParseResult<HighlightResult<'a>> {
        let highlighter = TreeHighlighter::new();
        highlighter.highlight(source, SolidityLanguage::NAME, theme)
    }
}
