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

pub use data_text_core::TextError;



pub use lexer::token_type::AplTokenType as TokenType;
pub use parser::element_type::AplElementType as ElementType;

pub use language::AplLanguage;
pub use lexer::AplLexer;
pub use parser::AplParser;
