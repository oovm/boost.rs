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

pub use crate::ast::TypstRoot;
pub use builder::TypstBuilder;
pub use language::TypstLanguage;
/// Re-export of lexer types.
pub use lexer::TypstLexer;
pub use parser::TypstParser;



pub use lexer::token_type::TypstTokenType;
pub use parser::element_type::TypstElementType;
