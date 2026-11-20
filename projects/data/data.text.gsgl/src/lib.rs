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
/// Syntax module.
pub use crate::ast::GsglRoot;
pub use crate::{builder::GsglBuilder, language::GsglLanguage, lexer::GsglLexer, parser::GsglParser};



pub use lexer::token_type::GsglTokenType;
pub use parser::element_type::GsglElementType;
