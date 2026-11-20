#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::VerilogHighlighter;
pub use crate::lsp::VerilogLanguageService;
pub use crate::lsp::formatter::VerilogFormatter;
pub use crate::mcp::serve_verilog_mcp;
