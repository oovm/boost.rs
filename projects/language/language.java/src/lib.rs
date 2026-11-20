#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::JavaHighlighter;
pub use crate::lsp::JavaLanguageService;
pub use crate::mcp::serve_java_mcp;
