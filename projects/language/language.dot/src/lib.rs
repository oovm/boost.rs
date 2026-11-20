#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::DotHighlighter;
pub use crate::lsp::DotLanguageService;
pub use crate::mcp::serve_dot_mcp;
