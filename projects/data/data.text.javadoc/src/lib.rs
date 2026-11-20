#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
mod builder;
/// Kind module.
/// Language module.
pub mod language;
/// Lexer module.
pub mod lexer;

/// Parser module.
pub mod parser;

pub use crate::{ast::JavadocRoot, builder::JavadocBuilder, language::JavadocLanguage, lexer::JavadocLexer, parser::JavadocParser};


pub use lexer::token_type::JavadocTokenType;
pub use parser::element_type::JavadocElementType;
