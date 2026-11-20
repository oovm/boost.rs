#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
pub use crate::lsp::highlighter::JinjaHighlighter;
pub use crate::lsp::JinjaLanguageService;
pub use crate::lsp::formatter::JinjaFormatter;
