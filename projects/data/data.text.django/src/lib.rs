#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod ast;
pub mod language;
pub mod lexer;
pub mod parser;

pub use crate::{
    ast::DjangoRoot,
    language::DjangoLanguage,
    lexer::{DjangoLexer, token_type::DjangoTokenType},
    parser::{DjangoParser, element_type::DjangoElementType},
};
