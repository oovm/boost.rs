#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::PhpHighlighter;
pub use crate::lsp::PhpLanguageService;
pub use crate::mcp::serve_php_mcp;
