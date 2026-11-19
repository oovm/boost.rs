#![doc = include_str!("readme.md")]

use data_text_core::errors::ParseResult;
use language_highlight::{
    syntax::{HighlightResult, HighlightSource, TreeHighlighter},
    themes::Theme,
};
/// Syntax highlighter for regular expressions.
pub struct RegexHighlighter {}

impl RegexHighlighter {
    /// Create a new instance of the regular expression highlighter.
    pub fn new() -> Self {
        Self {}
    }
}

impl HighlightSource for RegexHighlighter {
    fn highlight<'a>(&self, source: &'a str, language: &str, theme: Theme) -> ParseResult<HighlightResult<'a>> {
        let highlighter = TreeHighlighter::new();
        highlighter.highlight(source, language, theme)
    }
}
