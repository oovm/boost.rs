#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module for Perl.
pub mod ast;
/// Builder module for Perl.
pub mod builder;
/// Language configuration module for Perl.
pub mod language;
/// Lexer module for Perl.
pub mod lexer;
/// LSP module for Perl.
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
/// MCP module.
#[cfg(feature = "mcp")]
pub mod mcp;

/// Parser module for Perl.
pub mod parser;

pub use crate::{ast::PerlRoot, builder::PerlBuilder, language::PerlLanguage, lexer::PerlLexer, parser::PerlParser};

/// Highlighter implementation for Perl.
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::PerlHighlighter;

/// LSP implementation for Perl.
#[cfg(feature = "lsp")]
pub use crate::lsp::PerlLanguageService;
/// LSP formatter implementation for Perl.
#[cfg(feature = "lsp")]
pub use crate::lsp::formatter::PerlFormatter;

/// Perl's MCP service implementation.
#[cfg(feature = "mcp")]
pub use crate::mcp::serve_perl_mcp;
/// Perl token type.
pub use lexer::token_type::PerlTokenType;
/// Perl element type.
pub use parser::element_type::PerlElementType;
