#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::formatter::TailwindFormatter;
pub use crate::lsp::highlighter::TailwindHighlighter;
pub use crate::lsp::TailwindLanguageService;
pub use crate::mcp::serve_tailwind_mcp;
