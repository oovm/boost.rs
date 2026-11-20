#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::TypstHighlighter;
pub use crate::lsp::TypstLanguageService;
pub use crate::lsp::formatter::TypstFormatter;
pub use crate::mcp::serve_typst_mcp;
