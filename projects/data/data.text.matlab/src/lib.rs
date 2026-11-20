#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod ast;
/// Matlab AST building module
pub mod builder;

pub mod language;
pub mod lexer;

/// Matlab parsing module
pub mod parser;

pub use crate::{
    ast::MatlabRoot,
    builder::MatlabBuilder,
    language::MatlabLanguage,
    lexer::{MatlabLexer, token_type::MatlabTokenType},
    parser::{MatlabParser, element_type::MatlabElementType},
};



