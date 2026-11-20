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

pub use crate::{ast::PythonRoot, builder::PythonBuilder, language::PythonLanguage, lexer::PythonLexer, parser::PythonParser};

/// Python token type.
pub use lexer::token_type::PythonTokenType;
/// Python element type.
pub use parser::PythonElementType;
