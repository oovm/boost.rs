#![doc = include_str!("readme.md")]
#![deny(missing_docs)]
#![allow(missing_copy_implementations)]

/// The Markdown language implementation for `data.text`.
pub mod language;

/// Abstract Syntax Tree for Markdown.
pub mod ast;
/// Builder for constructing Markdown syntax trees.
pub mod builder;

/// Syntax kinds for Markdown.
/// Lexer for tokenizing Markdown source.
pub mod lexer;
/// Language Server Protocol support for Markdown.
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
/// MCP module.
#[cfg(feature = "mcp")]
pub mod mcp;

/// Parser for building Markdown syntax trees.
pub mod parser;

pub use crate::{ast::MarkdownRoot, builder::MarkdownBuilder, language::MarkdownLanguage, lexer::MarkdownLexer, parser::MarkdownParser};

#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::MarkdownHighlighter;

// #[cfg(feature = "mcp")]
// pub use crate::mcp::serve_markdown_mcp;
pub use lexer::token_type::MarkdownTokenType;
pub use parser::element_type::MarkdownElementType;
