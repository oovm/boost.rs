#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::ScssHighlighter;
pub use crate::lsp::ScssLanguageService;
pub use crate::lsp::formatter::ScssFormatter;
pub use crate::mcp::serve_scss_mcp;
