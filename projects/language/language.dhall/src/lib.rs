#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
pub use crate::lsp::highlighter::DHallHighlighter;
pub use crate::lsp::DHallLanguageService;
pub use crate::lsp::formatter::DHallFormatter;
