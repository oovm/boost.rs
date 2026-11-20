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

pub use crate::{ast::RRoot, builder::RBuilder, language::RLanguage, lexer::RLexer, parser::RParser};



pub use lexer::token_type::RTokenType;
pub use parser::element_type::RElementType;
