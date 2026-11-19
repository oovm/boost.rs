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

pub use crate::{ast::CppRoot, builder::CppBuilder, language::CppLanguage, lexer::CppLexer, parser::CppParser};
pub use lexer::token_type::CppTokenType;
pub use parser::element_type::CppElementType;
