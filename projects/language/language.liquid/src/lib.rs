#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
pub use crate::lsp::highlighter::LiquidHighlighter;
pub use crate::lsp::LiquidLanguageService;
pub use crate::lsp::formatter::LiquidFormatter;
