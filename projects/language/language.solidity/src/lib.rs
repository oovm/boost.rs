#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::SolidityHighlighter;
pub use crate::lsp::SolidityLanguageService;
pub use crate::lsp::formatter::SolidityFormatter;
pub use crate::mcp::serve_solidity_mcp;
