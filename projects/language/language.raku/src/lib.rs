#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::RakuHighlighter;
pub use crate::lsp::RakuLanguageService;
pub use crate::lsp::formatter::RakuFormatter;
pub use crate::mcp::serve_raku_mcp;
