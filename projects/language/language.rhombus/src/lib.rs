#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::RhombusLanguageService;
pub use crate::lsp::formatter::RhombusFormatter;
pub use crate::lsp::highlighter::RhombusHighlighter;
pub use crate::mcp::serve_rhombus_mcp;
