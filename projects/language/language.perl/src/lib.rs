#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::PerlHighlighter;
pub use crate::lsp::PerlLanguageService;
pub use crate::lsp::formatter::PerlFormatter;
pub use crate::mcp::serve_perl_mcp;
