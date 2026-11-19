#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;

/// Syntax kind module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;
/// Syntax module.
/// LSP module.
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
/// MCP module.
#[cfg(feature = "mcp")]
pub mod mcp;
/// Parser module.
pub mod parser;

pub use crate::{ast::ScalaRoot, builder::ScalaBuilder, language::ScalaLanguage, lexer::ScalaLexer, parser::ScalaParser};

/// Highlighter implementation.
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::ScalaHighlighter;

#[cfg(feature = "lsp")]
pub use crate::lsp::ScalaLanguageService;
/// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::formatter::ScalaFormatter;

/// MCP service implementation.
#[cfg(feature = "mcp")]
pub use crate::mcp::serve_scala_mcp;
pub use data_text_core::{ElementType, TokenType};
