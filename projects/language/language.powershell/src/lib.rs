#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::{HighlightKind, Highlighter, PowerShellHighlighter};
pub use crate::lsp::PowerShellLanguageService;
pub use crate::mcp::serve_powershell_mcp;
