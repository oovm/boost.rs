#![doc = include_str!("readme.md")]


use language_highlight::syntax::{HighlightSegment, HighlightSource};
use crate::ast::GoRoot;

/// Go syntax highlighter.
pub struct GoHighlighter;

impl GoHighlighter {
    pub fn highlight<'a>(&self, _root: &'a GoRoot) -> Vec<HighlightSegment<'_>> {
        vec![]
    }
}
