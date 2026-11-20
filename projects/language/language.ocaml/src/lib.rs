#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::{HighlightKind, Highlighter, OCamlHighlighter};
pub use crate::lsp::OCamlLanguageService;
pub use crate::mcp::serve_ocaml_mcp;
