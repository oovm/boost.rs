#![doc = include_str!("readme.md")]

use data_text_raku::ast::RakuRoot;

/// Raku syntax highlighter
pub struct RakuHighlighter;

impl RakuHighlighter {
    /// Creates a new Raku highlighter
    pub fn new() -> Self {
        Self
    }

    /// Highlights the Raku AST
    pub fn highlight(&self, _root: &RakuRoot) -> Vec<language_server::LspRange> {
        Vec::new()
    }
}

impl Default for RakuHighlighter {
    fn default() -> Self {
        Self::new()
    }
}
