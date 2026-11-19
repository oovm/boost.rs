#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;
/// Language module.
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

pub use crate::{language::VerilogLanguage, lexer::VerilogLexer, parser::VerilogParser};

/// Highlighter implementation.
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::VerilogHighlighter;

/// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::VerilogLanguageService;
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub use crate::lsp::formatter::VerilogFormatter;

/// MCP service implementation.
#[cfg(feature = "mcp")]
pub use crate::mcp::serve_verilog_mcp;

pub use crate::{lexer::token_type::VerilogKind as TokenType, parser::element_type::VerilogElementType as ElementType};
