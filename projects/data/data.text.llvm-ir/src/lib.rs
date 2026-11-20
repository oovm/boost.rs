#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod ast;
mod builder;

pub mod language;
pub mod lexer;

pub mod parser;

pub use crate::{
    ast::LLirRoot,
    builder::LLirBuilder,
    language::LLvmLanguage,
    lexer::{LLvmLexer, token_type::LLvmTokenType},
    parser::LLirParser,
};


pub use parser::element_type::LLvmElementType;
