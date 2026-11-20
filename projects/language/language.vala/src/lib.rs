#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::ValaHighlighter;
pub use crate::lsp::ValaLanguageService;
pub use crate::lsp::formatter::ValaFormatter;
pub use crate::mcp::serve_vala_mcp;
