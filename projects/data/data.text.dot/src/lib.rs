#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;

/// Kind definition module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;

/// Parser module.
pub mod parser;

pub use crate::{builder::DotBuilder, language::DotLanguage, lexer::DotLexer, parser::DotParser};



/// Dot token type.
pub use lexer::token_type::DotTokenType;
/// Dot element type.
pub use parser::element_type::DotElementType;
