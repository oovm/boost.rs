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

/// Core exports for the Vampire language.
pub use crate::{ast::VampireRoot, language::VampireLanguage, lexer::VampireLexer, parser::VampireParser};

/// Core trait exports from data_text_core.
pub use data_text_core::{ElementType, TokenType};

/// Highlighter implementation.
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::VampireHighlighter;

/// Language Server Protocol service for Vampire.
#[cfg(feature = "lsp")]
pub use crate::lsp::VampireLanguageService;
/// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::formatter::VampireFormatter;

/// MCP service implementation.
#[cfg(feature = "mcp")]
pub use crate::mcp::serve_vampire_mcp;
/// Token type enumeration for Vampire lexer.
pub use lexer::token_type::VampireTokenType;
/// Element type enumeration for Vampire parser.
pub use parser::element_type::VampireElementType;
