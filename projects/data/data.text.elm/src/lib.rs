#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module for Elm.
pub mod ast;
/// Builder module for Elm.
pub mod builder;
/// Language configuration for Elm.
pub mod language;
/// Lexer for Elm.
pub mod lexer;
/// LSP support for Elm.
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
/// Parser for Elm.
pub mod parser;

pub use crate::{
    ast::ElmRoot,
    builder::ElmBuilder,
    language::ElmLanguage,
    lexer::{ElmLexer, token_type::ElmTokenType},
    parser::element_type::ElmElementType,
};
