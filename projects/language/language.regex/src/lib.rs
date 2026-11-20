#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::RegexHighlighter;
pub use crate::lsp::RegexLanguageService;
pub use crate::lsp::formatter::RegexFormatter;
pub use crate::mcp::serve_regex_mcp;
