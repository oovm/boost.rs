#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;

/// Highlighter module.
#[cfg(feature = "lsp")]
pub use crate::lsp::highlighter;
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
/// Syntax module.
pub use crate::ast::GsglRoot;
pub use crate::{builder::GsglBuilder, language::GsglLanguage, lexer::GsglLexer, parser::GsglParser};

/// Highlighter implementation.
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::GsglHighlighter;

/// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::GsglLanguageService;

/// MCP service implementation.
#[cfg(feature = "mcp")]
pub use crate::mcp::serve_gsgl_mcp;
pub use lexer::token_type::GsglTokenType;
pub use parser::element_type::GsglElementType;
