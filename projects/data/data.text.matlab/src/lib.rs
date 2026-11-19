#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod ast;
/// Matlab AST building module
pub mod builder;

pub mod language;
pub mod lexer;
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
/// MCP module.
#[cfg(feature = "mcp")]
pub mod mcp;

/// Matlab parsing module
pub mod parser;

pub use crate::{
    ast::MatlabRoot,
    builder::MatlabBuilder,
    language::MatlabLanguage,
    lexer::{MatlabLexer, token_type::MatlabTokenType},
    parser::{MatlabParser, element_type::MatlabElementType},
};

#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::MatlabHighlighter;

#[cfg(feature = "lsp")]
pub use crate::lsp::MatlabLanguageService;

#[cfg(feature = "mcp")]
pub use crate::mcp::serve_matlab_mcp;
