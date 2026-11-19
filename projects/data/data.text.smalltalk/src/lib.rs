#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
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

pub use crate::{ast::SmalltalkRoot, language::SmalltalkLanguage, lexer::SmalltalkLexer, parser::SmalltalkParser};

pub use data_text_core::{ElementType, TokenType};

/// Highlighter implementation.
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::SmalltalkHighlighter;

#[cfg(feature = "lsp")]
pub use crate::lsp::SmalltalkLanguageService;
/// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::formatter::SmalltalkFormatter;

/// MCP service implementation.
#[cfg(feature = "mcp")]
pub use crate::mcp::serve_smalltalk_mcp;
pub use lexer::token_type::SmalltalkTokenType;
pub use parser::element_type::SmalltalkElementType;
