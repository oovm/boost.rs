#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
pub mod builder;
mod language;
mod lexer;
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
pub mod parser;

pub use crate::{
    ast::ElixirRoot,
    builder::ElixirBuilder,
    language::ElixirLanguage,
    lexer::{ElixirLexer, token_type::ElixirTokenType},
    parser::element_type::ElixirElementType,
};
