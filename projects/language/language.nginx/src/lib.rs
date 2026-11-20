#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::NginxHighlighter;
pub use crate::lsp::NginxLanguageService;
pub use crate::mcp::serve_nginx_mcp;
