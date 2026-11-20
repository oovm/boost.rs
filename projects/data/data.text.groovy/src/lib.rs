#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod ast;
pub mod builder;
pub mod language;
pub mod lexer;
pub mod parser;
pub use crate::{ast::GroovyRoot, builder::GroovyBuilder, language::GroovyLanguage, lexer::GroovyLexer, parser::GroovyParser};
pub use lexer::token_type::GroovyTokenType;
pub use parser::element_type::GroovyElementType;
