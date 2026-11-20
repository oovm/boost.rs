#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::TwigHighlighter;
pub use crate::lsp::TwigLanguageService;
pub use crate::lsp::formatter::TwigFormatter;
pub use crate::mcp::serve_twig_mcp;
