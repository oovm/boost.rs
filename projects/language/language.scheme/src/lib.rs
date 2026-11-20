#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::SchemeLanguageService;
pub use crate::lsp::formatter::SchemeFormatter;
pub use crate::lsp::highlighter::SchemeHighlighter;
pub use crate::mcp::serve_scheme_mcp;
