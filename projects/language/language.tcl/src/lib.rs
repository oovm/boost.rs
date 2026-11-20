#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::TclHighlighter;
pub use crate::lsp::TclLanguageService;
pub use crate::lsp::formatter::TclFormatter;
pub use crate::mcp::serve_tcl_mcp;
