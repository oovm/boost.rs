#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::VampireHighlighter;
pub use crate::lsp::VampireLanguageService;
pub use crate::lsp::formatter::VampireFormatter;
pub use crate::mcp::serve_vampire_mcp;
