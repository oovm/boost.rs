#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module
pub mod ast;
/// Builder module
pub mod builder;
mod language;
mod lexer;
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
/// LSP module
pub mod lsp;
/// Parser module
pub mod parser;

pub use ast::{JinjaElement, JinjaRoot};
pub use builder::JinjaBuilder;
pub use language::JinjaLanguage;
pub use lexer::{JinjaLexer, token_type::JinjaTokenType};
pub use parser::{JinjaParser, element_type::JinjaElementType};

/// Highlighter implementation
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::JinjaHighlighter;

#[cfg(feature = "lsp")]
pub use crate::lsp::JinjaLanguageService;
/// LSP implementation
#[cfg(feature = "lsp")]
pub use crate::lsp::formatter::JinjaFormatter;
