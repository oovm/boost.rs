#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;
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
    ast::StylusRoot,
    builder::StylusBuilder,
    language::StylusLanguage,
    lexer::{StylusLexer, token_type::StylusTokenType},
    parser::StylusParser,
};
