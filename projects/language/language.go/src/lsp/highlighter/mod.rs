#![doc = include_str!("readme.md")]

use data_text_go::ast::GoRoot;
use language_highlight::syntax::{HighlightSegment, HighlightSource};

/// Go syntax highlighter.
pub struct GoHighlighter;

impl GoHighlighter {
    pub fn highlight<'a>(&self, _root: &'a GoRoot) -> Vec<HighlightSegment<'_>> {
        vec![]
    }
}
