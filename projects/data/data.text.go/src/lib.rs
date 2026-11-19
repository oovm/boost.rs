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

pub use crate::ast::GoRoot;
pub use builder::GoBuilder;
pub use language::GoLanguage;
pub use lexer::GoLexer;
pub use parser::GoParser;

/// Highlighter implementation.
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::GoHighlighter;

/// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::GoLanguageService;
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub use crate::lsp::formatter::GoFormatter;

/// MCP service implementation.
#[cfg(feature = "mcp")]
pub use crate::mcp::serve_go_mcp;
pub use lexer::token_type::GoTokenType;
pub use parser::element_type::GoElementType;
