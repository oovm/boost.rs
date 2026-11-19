#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]
#![doc = include_str!("readme.md")]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;

// pub mod formatter;

/// Type definitions module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;
/// LSP module.
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
/// MCP module.
#[cfg(feature = "mcp")]
pub mod mcp;

/// Parser module.
pub mod parser;

pub use crate::{
    builder::{NimBuilder, NimRoot},
    language::NimLanguage,
    lexer::NimLexer,
    parser::NimParser,
};

/// Highlighter implementation.
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::NimHighlighter;

/// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::NimLanguageService;

/// MCP service implementation.
#[cfg(feature = "mcp")]
pub use crate::mcp::serve_nim_mcp;
pub use lexer::token_type::NimTokenType;
pub use parser::element_type::NimElementType;
