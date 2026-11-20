#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::RbqHighlighter;
pub use crate::lsp::formatter::RbqFormatter;
pub use crate::lsp::RbqLanguageService;
pub use crate::mcp::serve_rbq_mcp;
