#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
pub mod language;
pub mod lexer;
pub mod parser;

mod builder;

pub use crate::{ast::*, builder::SchemeBuilder, language::SchemeLanguage, lexer::SchemeLexer, parser::SchemeParser};


pub use lexer::token_type::SchemeTokenType;
pub use parser::element_type::SchemeElementType;
