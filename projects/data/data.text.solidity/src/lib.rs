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

pub use crate::{ast::SolidityRoot, language::SolidityLanguage, lexer::SolidityLexer, parser::SolidityParser};

pub use data_text_core::{ElementType, TokenType};



pub use lexer::token_type::SolidityTokenType;
pub use parser::element_type::SolidityElementType;
