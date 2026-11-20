#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod ast;
pub mod builder;
pub mod language;
pub mod lexer;
pub mod parser;

pub use crate::{
    ast::CrystalRoot,
    builder::CrystalBuilder,
    language::CrystalLanguage,
    lexer::{CrystalLexer, token_type::CrystalTokenType},
    parser::{CrystalParser, element_type::CrystalElementType},
};
