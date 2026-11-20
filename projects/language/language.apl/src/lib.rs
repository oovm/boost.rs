#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::AplHighlighter;
pub use crate::lsp::AplLanguageService;
pub use crate::lsp::formatter::AplFormatter;
pub use crate::mcp::serve_apl_mcp;
