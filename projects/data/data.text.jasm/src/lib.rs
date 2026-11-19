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
/// Syntax module.
pub use crate::{ast::JasmRoot, builder::JasmBuilder, language::JasmLanguage, lexer::JasmLexer, parser::JasmParser};

/// Highlighter implementation.
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::JasmHighlighter;

/// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::JasmLanguageService;
/// Formatter implementation.
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub use crate::lsp::formatter::JasmFormatter;

/// MCP service implementation.
#[cfg(feature = "mcp")]
pub use crate::mcp::serve_jasm_mcp;
/// Jasm token type.
pub use lexer::token_type::JasmTokenType;
/// Jasm element type.
pub use parser::element_type::JasmElementType;
