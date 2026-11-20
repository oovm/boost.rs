#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::formatter::ActionScriptFormatter;
pub use crate::lsp::highlighter::ActionScriptHighlighter;
pub use crate::lsp::ActionScriptLanguageService;
pub use crate::mcp::serve_actionscript_mcp;
