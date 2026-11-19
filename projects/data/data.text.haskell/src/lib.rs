#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// Abstract Syntax Tree (AST) definitions for Haskell.
pub mod ast;
/// Incremental tree builder for Haskell.
pub mod builder;

mod language;
mod lexer;
/// Language Server Protocol (LSP) and editor integration for Haskell.
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
/// Model Context Protocol (MCP) support for Haskell.
#[cfg(feature = "mcp")]
pub mod mcp;

mod parser;

pub use crate::{ast::HaskellRoot, builder::HaskellBuilder, language::HaskellLanguage, lexer::HaskellLexer, parser::HaskellParser};

#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::HaskellHighlighter;

#[cfg(feature = "lsp")]
pub use crate::lsp::HaskellLanguageService;

#[cfg(feature = "mcp")]
pub use crate::mcp::serve_haskell_mcp;
/// Haskell token types.
pub use lexer::token_type::{HaskellToken, HaskellTokenType};
/// Haskell element types.
pub use parser::element_type::HaskellElementType;
