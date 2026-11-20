#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::SwiftHighlighter;
pub use crate::lsp::SwiftLanguageService;
pub use crate::lsp::formatter::SwiftFormatter;
pub use crate::mcp::serve_swift_mcp;
