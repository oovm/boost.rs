#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
/// formatter module.
pub mod formatter;
pub use crate::lsp::highlighter::JasmHighlighter;
pub use crate::lsp::JasmLanguageService;
pub use crate::formatter::JasmFormatter;
pub use crate::mcp::serve_jasm_mcp;
