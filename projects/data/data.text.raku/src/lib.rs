#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module for Raku.
pub mod ast;
/// Builder module for Raku.
pub mod builder;
/// Language configuration module for Raku.
pub mod language;
/// Lexer module for Raku tokenization.
pub mod lexer;

/// Parser module for Raku syntax analysis.
pub mod parser;

/// LSP module for Raku.
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
/// MCP module for Raku.
#[cfg(feature = "mcp")]
pub mod mcp;

pub use crate::{ast::RakuRoot, builder::RakuBuilder, language::RakuLanguage, lexer::RakuLexer, parser::RakuParser};

/// Highlighter implementation for Raku.
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::RakuHighlighter;

#[cfg(feature = "lsp")]
pub use crate::lsp::RakuLanguageService;
/// LSP implementation for Raku.
#[cfg(feature = "lsp")]
pub use crate::lsp::formatter::RakuFormatter;

/// MCP service implementation for Raku.
#[cfg(feature = "mcp")]
pub use crate::mcp::serve_raku_mcp;

pub use lexer::token_type::RakuTokenType;
pub use parser::element_type::RakuElementType;
