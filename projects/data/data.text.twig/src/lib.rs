#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
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

pub use crate::ast::TwigRoot;
pub use builder::TwigBuilder;
pub use language::TwigLanguage;
pub use lexer::TwigLexer;
pub use parser::TwigParser;

/// Highlighter implementation.
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::TwigHighlighter;

#[cfg(feature = "lsp")]
pub use crate::lsp::TwigLanguageService;
/// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::formatter::TwigFormatter;

/// MCP service implementation.
#[cfg(feature = "mcp")]
pub use crate::mcp::serve_twig_mcp;
pub use lexer::token_type::TwigTokenType;
pub use parser::element_type::TwigElementType;
