#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;

/// Type definitions module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;

/// Parser module.
pub mod parser;

pub use crate::{ast::MsilRoot, builder::MsilBuilder, language::MsilLanguage, lexer::MsilLexer, parser::MsilParser};



pub use lexer::token_type::MsilTokenType;
pub use parser::element_type::MsilElementType;
