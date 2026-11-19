#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod builder;
pub mod language;
pub mod lexer;
pub mod parser;

pub use crate::{builder::AthenaBuilder, language::AthenaLanguage, lexer::AthenaLexer, parser::AthenaParser};
pub use lexer::token_type::AthenaTokenType;
pub use data_text_core::{ElementType, TokenType};
pub use parser::element_type::AthenaElementType;
