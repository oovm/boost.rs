#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
pub use crate::lsp::highlighter;
pub use crate::lsp::highlighter::GsglHighlighter;
pub use crate::lsp::GsglLanguageService;
