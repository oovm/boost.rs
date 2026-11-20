#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// Abstract Syntax Tree types.
pub mod ast;
/// AST builder.
pub mod builder;
/// Language definition.
pub mod language;
/// Lexer for tokenization.
pub mod lexer;
/// Parser for syntax analysis.
pub mod parser;

pub use language::RacketLanguage;
