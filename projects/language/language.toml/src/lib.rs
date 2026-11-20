#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::TomlHighlighter;
pub use crate::lsp::TomlLanguageService;
pub use crate::lsp::formatter::TomlFormatter;
