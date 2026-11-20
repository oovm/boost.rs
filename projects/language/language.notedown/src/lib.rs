#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
/// mcp module.
pub mod mcp;
pub use crate::lsp::highlighter::{HighlightKind, Highlighter, NoteHighlighter};
pub use crate::lsp::NoteLanguageService;
pub use crate::mcp::serve_note_mcp;
