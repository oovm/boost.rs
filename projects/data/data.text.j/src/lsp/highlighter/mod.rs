#![doc = include_str!("readme.md")]

use language_highlight::syntax::{HighlightResult, HighlightSource};
use language_highlight::themes::Theme;

/// Local definition of highlight kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HighlightKind {
    /// Keywords (corresponding to primitives in J).
    Keyword,
    /// Strings.
    String,
    /// Numbers.
    Number,
    /// Comments.
    Comment,
    /// Identifiers.
    Identifier,
}

/// Highlighter trait.
pub trait Highlighter {
    /// Highlights the given text.
    fn highlight(&self, text: &str) -> Vec<(usize, usize, HighlightKind)>;
}

/// J syntax highlighter.
pub struct JHighlighter {
    /// Whether to use parser-based highlighting.
    pub use_parser: bool,
}

impl Default for JHighlighter {
    fn default() -> Self {
        Self { use_parser: false }
    }
}

impl JHighlighter {
    /// Creates a new J highlighter instance.
    pub fn new() -> Self {
        Self::default()
    }

    /// Highlights J primitives (corresponding to keywords in other languages).
    fn highlight_keywords(&self, text: &str) -> Vec<(usize, usize, HighlightKind)> {
        let mut highlights = Vec::new();
        // J assignment symbols.
        for op in ["=:", "=."] {
            let mut start = 0;
            while let Some(pos) = text[start..].find(op) {
                highlights.push((start + pos, start + pos + op.len(), HighlightKind::Keyword));
                start += pos + op.len();
            }
        }
        highlights
    }
}

impl Highlighter for JHighlighter {
    fn highlight(&self, text: &str) -> Vec<(usize, usize, HighlightKind)> {
        let mut highlights = self.highlight_keywords(text);

        // Simple regex or manual scanning logic can be added here.
        // For simplicity, we currently rely mainly on the lexer for highlighting.

        highlights.sort_by_key(|h| h.0);
        highlights
    }
}

#[cfg(feature = "language-highlight")]
impl HighlightSource for JHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, _theme: Theme) -> data_text_core::errors::ParseResult<HighlightResult<'a>> {
        Ok(HighlightResult { segments: Vec::new(), source: std::borrow::Cow::Borrowed(source) })
    }
}
