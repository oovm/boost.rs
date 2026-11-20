#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::LeanHighlighter;
pub use crate::lsp::LeanLanguageService;
pub use crate::mcp::serve_lean_mcp;
