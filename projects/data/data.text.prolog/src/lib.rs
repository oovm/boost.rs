#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;

/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;

/// Parser module.
pub mod parser;

pub use crate::{ast::PrologRoot, builder::PrologBuilder, language::PrologLanguage, lexer::token_type::PrologTokenType, parser::PrologParser};



pub use parser::element_type::PrologElementType;
