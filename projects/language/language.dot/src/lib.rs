#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
pub use crate::lsp::highlighter::DotHighlighter;
pub use crate::lsp::DotLanguageService;
