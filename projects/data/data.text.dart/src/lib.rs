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

pub use crate::{language::DartLanguage, lexer::DartLexer, parser::DartParser};

//

// /// LSP implementation.
/// Token type for Dart lexer.
pub use lexer::token_type::DartTokenType;
/// Element type for Dart parser.
pub use parser::element_type::DartElementType;
