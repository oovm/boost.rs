#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::AdaHighlighter;
pub use crate::lsp::AdaLanguageService;
pub use crate::lsp::formatter::AdaFormatter;
pub use crate::mcp::serve_ada_mcp;
