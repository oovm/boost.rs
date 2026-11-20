#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module for Pascal.
pub mod ast;
/// Builder module for Pascal.
pub mod builder;

/// Language configuration module for Pascal.
pub mod language;
/// Lexer module for Pascal tokenization.
pub mod lexer;

/// Parser module for Pascal syntax analysis.
pub mod parser;

pub use crate::{ast::PascalRoot, builder::PascalBuilder, language::PascalLanguage, lexer::PascalLexer, parser::PascalParser};



pub use lexer::token_type::PascalTokenType;
pub use parser::element_type::PascalElementType;
