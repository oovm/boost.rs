#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::SassHighlighter;
pub use crate::lsp::SassLanguageService;
pub use crate::lsp::formatter::SassFormatter;
pub use crate::mcp::serve_sass_mcp;
