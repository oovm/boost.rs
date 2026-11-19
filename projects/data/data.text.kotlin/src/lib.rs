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
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
/// MCP module.
#[cfg(feature = "mcp")]
pub mod mcp;

/// Parser module.
pub mod parser;

pub use crate::{ast::KotlinRoot, builder::KotlinBuilder, language::KotlinLanguage, lexer::KotlinLexer, parser::KotlinParser};

#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::KotlinHighlighter;

#[cfg(feature = "lsp")]
pub use crate::lsp::KotlinLanguageService;

#[cfg(feature = "mcp")]
pub use crate::mcp::serve_kotlin_mcp;
pub use lexer::token_type::KotlinTokenType;
pub use parser::element_type::KotlinElementType;
