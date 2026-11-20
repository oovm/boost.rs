#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
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

pub use crate::ast::TwigRoot;
pub use builder::TwigBuilder;
pub use language::TwigLanguage;
pub use lexer::TwigLexer;
pub use parser::TwigParser;



pub use lexer::token_type::TwigTokenType;
pub use parser::element_type::TwigElementType;
