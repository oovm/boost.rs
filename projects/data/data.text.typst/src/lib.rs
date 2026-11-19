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

pub use crate::ast::TypstRoot;
pub use builder::TypstBuilder;
pub use language::TypstLanguage;
/// Re-export of lexer types.
pub use lexer::TypstLexer;
pub use parser::TypstParser;

/// Highlighter implementation.
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::TypstHighlighter;

#[cfg(feature = "lsp")]
pub use crate::lsp::TypstLanguageService;
/// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::formatter::TypstFormatter;

/// MCP service implementation.
#[cfg(feature = "mcp")]
pub use crate::mcp::serve_typst_mcp;
pub use lexer::token_type::TypstTokenType;
pub use parser::element_type::TypstElementType;
