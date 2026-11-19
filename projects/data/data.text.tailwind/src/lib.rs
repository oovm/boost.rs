#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;

/// Builder module.
pub mod builder;
/// Language configuration.
pub mod language;
/// Lexer implementation.
pub mod lexer;
/// Parser implementation.
pub mod parser;

/// LSP support.
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
/// MCP module.
#[cfg(feature = "mcp")]
pub mod mcp;

#[cfg(feature = "mcp")]
/// Tailwind root node.
pub use crate::ast::TailwindRoot;
pub use crate::{builder::TailwindBuilder, language::TailwindLanguage, lexer::TailwindLexer, parser::TailwindParser};

/// Formatter implementation.
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub use crate::lsp::formatter::TailwindFormatter;

/// Highlighter implementation.
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::TailwindHighlighter;

/// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::TailwindLanguageService;

/// MCP service implementation.
#[cfg(feature = "mcp")]
pub use crate::mcp::serve_tailwind_mcp;
/// Tailwind token type.
pub use lexer::token_type::TailwindTokenType;
/// Tailwind element type.
pub use parser::element_type::TailwindElementType;
