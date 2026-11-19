#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;

/// Kind definition module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;
/// LSP module.
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;

#[cfg(feature = "mcp")]
/// MCP module.
pub mod mcp;
/// Parser module.
pub mod parser;

pub use crate::{builder::DotBuilder, language::DotLanguage, lexer::DotLexer, parser::DotParser};

/// Highlighter implementation.
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::DotHighlighter;

/// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::DotLanguageService;

/// MCP service implementation.
#[cfg(feature = "mcp")]
pub use crate::mcp::serve_dot_mcp;
/// Dot token type.
pub use lexer::token_type::DotTokenType;
/// Dot element type.
pub use parser::element_type::DotElementType;
