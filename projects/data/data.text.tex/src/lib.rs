#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module containing TeX syntax tree definitions.
pub mod ast;
/// Builder module for constructing TeX ASTs.
pub mod builder;

/// Language definition and configuration for TeX.
pub mod language;
/// Lexer implementation for TeX.
pub mod lexer;
/// LSP-related functionality (hover, completion, highlighting) for TeX.
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
/// MCP (Model Context Protocol) integration for TeX.
#[cfg(feature = "mcp")]
pub mod mcp;

/// Parser implementation for TeX.
pub mod parser;

pub use crate::{ast::TexRoot, builder::TexBuilder, language::TexLanguage, lexer::TexLexer, parser::TexParser};

/// Highlighter implementation.
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::TexHighlighter;

#[cfg(feature = "lsp")]
pub use crate::lsp::TexLanguageService;
/// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::formatter::TexFormatter;

/// MCP service implementation.
#[cfg(feature = "mcp")]
pub use crate::mcp::serve_tex_mcp;
pub use lexer::token_type::TexTokenType;
pub use parser::element_type::TexElementType;
