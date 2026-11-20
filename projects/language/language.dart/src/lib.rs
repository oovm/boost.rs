#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
pub use crate::lsp::highlighter::DartHighlighter;
pub use crate::lsp::DartLanguageService;
pub use crate::lsp::formatter::DartFormatter;
