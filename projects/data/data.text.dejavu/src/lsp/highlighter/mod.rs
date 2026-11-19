#![doc = include_str!("readme.md")]

use data_text_core::errors::ParseResult;
use language_highlight::syntax::{HighlightSource, HighlightResult, TreeHighlighter};
use language_highlight::themes::Theme;
/// A syntax highlighter for Dejavu source code.
pub struct DejavuHighlighter {
    /// Whether to use parser-based highlighting for enhanced accuracy
    pub use_parser: bool,
}

impl Default for DejavuHighlighter {
    fn default() -> Self {
        Self { use_parser: true }
    }
}

impl DejavuHighlighter {
    /// Creates a new Dejavu highlighter.
    pub fn new() -> Self {
        Self::default()
    }
}

impl HighlightSource for DejavuHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, theme: Theme) -> ParseResult<HighlightResult<'a>> {
        let highlighter = TreeHighlighter::new().theme(theme);

        if self.use_parser {
            let config = crate::DejavuLanguage::default();
            let parser = crate::DejavuParser::new(&config);
            let lexer = crate::DejavuLexer::new(&config);
            highlighter.highlight_with_language(source, theme, &parser, &lexer)
        }
        else {
            highlighter.highlight(source, "dejavu", theme)
        }
    }
}
