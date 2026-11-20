#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::KotlinHighlighter;
pub use crate::lsp::KotlinLanguageService;
pub use crate::mcp::serve_kotlin_mcp;
