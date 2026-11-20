#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::TexHighlighter;
pub use crate::lsp::TexLanguageService;
pub use crate::lsp::formatter::TexFormatter;
pub use crate::mcp::serve_tex_mcp;
