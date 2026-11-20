#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod ast;
mod builder;
pub mod language;
pub mod lexer;
pub mod parser;

pub use crate::{ast::WolframRoot, builder::WolframBuilder, language::WolframLanguage, lexer::WolframLexer, parser::WolframParser};



pub use data_text_core::{ElementType, TokenType};
pub use lexer::token_type::WolframTokenType;
pub use parser::element_type::WolframElementType;
