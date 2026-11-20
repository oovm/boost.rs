#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;
/// Parser module.
pub mod parser;

pub use crate::{
    ast::CoqRoot,
    builder::CoqBuilder,
    language::CoqLanguage,
    lexer::CoqLexer,
    parser::{CoqParser, element_type::CoqElementType},
};
pub use lexer::token_type::CoqTokenType;



