#![doc = include_str!("readme.md")]

use data_text_core::errors::ParseResult;
use language_highlight::{
    syntax::{HighlightResult, HighlightSource, TreeHighlighter},
    themes::Theme,
};
/// Vampire syntax highlighter
pub struct VampireHighlighter;

impl HighlightSource for VampireHighlighter {
    fn highlight<'a>(&self, source: &'a str, language: &str, theme: Theme) -> ParseResult<HighlightResult<'a>> {
        let highlighter = TreeHighlighter::new().theme(theme);
        highlighter.highlight(source, language, theme)
    }
}
