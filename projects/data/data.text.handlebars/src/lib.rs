#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod ast;
/// AST Builder
pub mod builder;

pub mod language;
pub mod lexer;

pub mod parser;

pub use crate::{
    ast::HandlebarsRoot,
    builder::HandlebarsBuilder,
    language::HandlebarsLanguage,
    lexer::{HandlebarsLexer, token_type::HandlebarsTokenType},
    parser::{HandlebarsParser, element_type::HandlebarsElementType},
};
