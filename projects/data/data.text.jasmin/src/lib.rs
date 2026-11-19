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

pub use crate::{ast::JasminRoot, builder::JasminBuilder, language::JasminLanguage, lexer::JasminLexer, parser::JasminParser};

#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::JasminHighlighter;

#[cfg(feature = "lsp")]
pub use crate::lsp::JasminLanguageService;
pub use lexer::token_type::JasminTokenType;
pub use parser::element_type::JasminElementType;
