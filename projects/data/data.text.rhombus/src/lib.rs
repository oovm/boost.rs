#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod ast;
pub mod language;
pub mod lexer;
pub mod parser;

mod builder;

pub use crate::{ast::*, builder::RhombusBuilder, language::RhombusLanguage, lexer::RhombusLexer, parser::RhombusParser};


pub use lexer::token_type::RhombusTokenType;
pub use parser::element_type::RhombusElementType;
