#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::JuliaHighlighter;
pub use crate::lsp::JuliaLanguageService;
pub use crate::mcp::serve_julia_mcp;
