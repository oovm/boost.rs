#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod ast;
pub mod builder;

pub mod language;
pub mod lexer;
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
/// MCP module.
#[cfg(feature = "mcp")]
pub mod mcp;

/// Parser module.
pub mod parser;

pub use crate::{
    ast::PowerShellRoot, builder::PowerShellBuilder, language::PowerShellLanguage, lexer::token_type::PowerShellTokenType,
    parser::PowerShellParser,
};

#[cfg(feature = "lsp")]
pub use crate::lsp::highlighter::{HighlightKind, Highlighter, PowerShellHighlighter};

#[cfg(feature = "lsp")]
pub use crate::lsp::PowerShellLanguageService;

#[cfg(feature = "mcp")]
pub use crate::mcp::serve_powershell_mcp;
pub use parser::element_type::PowerShellElementType;
