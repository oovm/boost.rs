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

pub use language::JLanguage;
pub use lexer::{JLexer, JTokenType};
pub use parser::{JElementType, JParser};

pub use data_text_core::{ElementType, Language, TokenType};

/// Highlighter implementation.
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::JHighlighter;

#[cfg(feature = "lsp")]
pub use crate::lsp::JLanguageService;
/// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::formatter::JFormatter;

/// MCP service implementation.
#[cfg(feature = "mcp")]
pub use crate::mcp::serve_j_mcp;
