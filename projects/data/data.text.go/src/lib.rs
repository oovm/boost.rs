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

pub use crate::ast::GoRoot;
pub use builder::GoBuilder;
pub use language::GoLanguage;
pub use lexer::GoLexer;
pub use parser::GoParser;



pub use lexer::token_type::GoTokenType;
pub use parser::element_type::GoElementType;
