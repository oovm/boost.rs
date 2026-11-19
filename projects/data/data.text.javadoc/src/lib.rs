#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
mod builder;
/// Kind module.
/// Language module.
pub mod language;
/// Lexer module.
pub mod lexer;
/// LSP and related services (HighlighterMCP).
#[cfg(any(feature = "lsp", feature = "language-highlight", feature = "mcp"))]
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
/// MCP module.
#[cfg(feature = "mcp")]
pub mod mcp;

/// Parser module.
pub mod parser;

pub use crate::{ast::JavadocRoot, builder::JavadocBuilder, language::JavadocLanguage, lexer::JavadocLexer, parser::JavadocParser};

/// Highlighter implementation.
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::JavadocHighlighter;

/// LSP service implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::JavadocLanguageService;
pub use lexer::token_type::JavadocTokenType;
pub use parser::element_type::JavadocElementType;
