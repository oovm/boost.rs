#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::GoHighlighter;
pub use crate::lsp::GoLanguageService;
pub use crate::lsp::formatter::GoFormatter;
pub use crate::mcp::serve_go_mcp;
