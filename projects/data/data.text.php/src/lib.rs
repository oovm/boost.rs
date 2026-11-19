#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;

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

pub use crate::{ast::PhpRoot, builder::PhpBuilder, language::PhpLanguage, lexer::PhpLexer, parser::PhpParser};

/// Highlighter implementation.
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::PhpHighlighter;

/// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::PhpLanguageService;

/// MCP service implementation.
#[cfg(feature = "mcp")]
pub use crate::mcp::serve_php_mcp;
pub use lexer::token_type::PhpTokenType;
pub use parser::element_type::PhpElementType;
