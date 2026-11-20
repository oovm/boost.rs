#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::CoqHighlighter;
pub use crate::lsp::CoqLanguageService;
pub use crate::lsp::formatter::CoqFormatter;
pub use crate::mcp::serve_coq_mcp;
