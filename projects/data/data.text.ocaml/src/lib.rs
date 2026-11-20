#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod ast;
/// Builder module for OCaml.
pub mod builder;
pub mod language;
pub mod lexer;

/// Parser module for OCaml.
pub mod parser;

pub use builder::OCamlBuilder;
pub use language::OCamlLanguage;
pub use lexer::OCamlLexer;
pub use parser::OCamlParser;



pub use lexer::token_type::OCamlTokenType;
pub use parser::element_type::OCamlElementType;
