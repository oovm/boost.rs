#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter;
pub use crate::lsp::highlighter::GsglHighlighter;
pub use crate::lsp::GsglLanguageService;
pub use crate::mcp::serve_gsgl_mcp;
