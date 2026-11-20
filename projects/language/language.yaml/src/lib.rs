#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::YamlHighlighter;
pub use crate::lsp::YamlLanguageService;
pub use crate::lsp::formatter::YamlFormatter;
pub use crate::mcp::serve_yaml_mcp;
