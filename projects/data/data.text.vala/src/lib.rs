#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;

/// Type definitions module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;

/// Parser module.
pub mod parser;

pub use crate::{
    ast::ValaRoot,
    builder::ValaBuilder,
    language::ValaLanguage,
    lexer::{ValaLexer, token_type::ValaTokenType},
    parser::{ValaParser, element_type::ValaElementType},
};



