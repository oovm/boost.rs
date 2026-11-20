#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;
/// Language module.
pub mod language;
/// Lexer module.
pub mod lexer;

/// Parser module.
pub mod parser;

pub use crate::{language::VerilogLanguage, lexer::VerilogLexer, parser::VerilogParser};




pub use crate::{lexer::token_type::VerilogKind as TokenType, parser::element_type::VerilogElementType as ElementType};
