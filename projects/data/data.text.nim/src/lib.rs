#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]
#![doc = include_str!("readme.md")]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;

// pub mod formatter;

/// Type definitions module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;

/// Parser module.
pub mod parser;

pub use crate::{
    builder::{NimBuilder, NimRoot},
    language::NimLanguage,
    lexer::NimLexer,
    parser::NimParser,
};



pub use lexer::token_type::NimTokenType;
pub use parser::element_type::NimElementType;
