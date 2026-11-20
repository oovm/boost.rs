#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::RHighlighter;
pub use crate::lsp::RLanguageService;
pub use crate::lsp::formatter::RFormatter;
pub use crate::mcp::serve_r_mcp;
