#![doc = include_str!("readme.md")]
use data_text_smalltalk::language::SmalltalkLanguage;
use data_text_core::RedNode;
/// Smalltalk Code Formatter
pub struct SmalltalkFormatter {
    /// Indentation size.
    indent_size: usize,
}

impl SmalltalkFormatter {
    pub fn new() -> Self {
        Self { indent_size: 4 }
    }

    pub fn format(&self, root: &RedNode<SmalltalkLanguage>, source: &str) -> String {
        // Simple formatting implementation
        source.to_string()
    }
}
