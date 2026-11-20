#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::PrologHighlighter;
pub use crate::lsp::PrologLanguageService;
pub use crate::mcp::serve_prolog_mcp;
