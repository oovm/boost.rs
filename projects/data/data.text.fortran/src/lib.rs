#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;
/// Language module.
pub mod language;
/// Lexer module.
pub mod lexer;
/// Parser module.
pub mod parser;

pub use crate::{
    ast::FortranRoot,
    builder::FortranBuilder,
    language::FortranLanguage,
    lexer::{FortranLexer, token_type::FortranTokenType},
    parser::element_type::FortranElementType,
};
