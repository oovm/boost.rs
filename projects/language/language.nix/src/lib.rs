#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::NixHighlighter;
pub use crate::lsp::NixLanguageService;
pub use crate::mcp::serve_nix_mcp;
