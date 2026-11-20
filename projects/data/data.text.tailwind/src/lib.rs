#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;

/// Builder module.
pub mod builder;
/// Language configuration.
pub mod language;
/// Lexer implementation.
pub mod lexer;
/// Parser implementation.
pub mod parser;


/// Tailwind root node.
pub use crate::ast::TailwindRoot;
pub use crate::{builder::TailwindBuilder, language::TailwindLanguage, lexer::TailwindLexer, parser::TailwindParser};




/// Tailwind token type.
pub use lexer::token_type::TailwindTokenType;
/// Tailwind element type.
pub use parser::element_type::TailwindElementType;
