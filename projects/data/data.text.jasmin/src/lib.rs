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

pub use crate::{ast::JasminRoot, builder::JasminBuilder, language::JasminLanguage, lexer::JasminLexer, parser::JasminParser};


pub use lexer::token_type::JasminTokenType;
pub use parser::element_type::JasminElementType;
