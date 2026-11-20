#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::NimHighlighter;
pub use crate::lsp::NimLanguageService;
pub use crate::mcp::serve_nim_mcp;
