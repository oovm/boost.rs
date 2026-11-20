#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::SmalltalkHighlighter;
pub use crate::lsp::SmalltalkLanguageService;
pub use crate::lsp::formatter::SmalltalkFormatter;
pub use crate::mcp::serve_smalltalk_mcp;
