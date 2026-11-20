#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
pub mod builder;
pub mod language;
pub mod lexer;
pub mod parser;

pub use crate::{
    ast::ElixirRoot,
    builder::ElixirBuilder,
    language::ElixirLanguage,
    lexer::{ElixirLexer, token_type::ElixirTokenType},
    parser::element_type::ElixirElementType,
};
