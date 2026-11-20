#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::ScalaHighlighter;
pub use crate::lsp::ScalaLanguageService;
pub use crate::lsp::formatter::ScalaFormatter;
pub use crate::mcp::serve_scala_mcp;
