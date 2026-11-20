#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::DejavuHighlighter;
pub use crate::lsp::DejavuLanguageService;
pub use crate::lsp::formatter::DejavuFormatter;
pub use crate::mcp::serve_dejavu_mcp;
