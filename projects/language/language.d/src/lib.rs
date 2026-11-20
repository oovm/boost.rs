#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
pub use crate::lsp::highlighter::DHighlighter;
pub use crate::lsp::DLanguageService;
pub use crate::lsp::formatter::DFormatter;
