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

pub use crate::{ast::KotlinRoot, builder::KotlinBuilder, language::KotlinLanguage, lexer::KotlinLexer, parser::KotlinParser};



pub use lexer::token_type::KotlinTokenType;
pub use parser::element_type::KotlinElementType;
