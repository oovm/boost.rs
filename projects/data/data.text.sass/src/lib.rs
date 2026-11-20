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
pub use crate::ast::SassRoot;
pub use crate::{builder::SassBuilder, language::SassLanguage, lexer::SassLexer, parser::SassParser};



pub use lexer::token_type::SassTokenType;
pub use parser::element_type::SassElementType;
