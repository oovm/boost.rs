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

pub use crate::{ast::RubyRoot, builder::RubyBuilder, language::RubyLanguage, lexer::RubyLexer, parser::RubyParser};
pub use lexer::token_type::RubyTokenType;
pub use parser::element_type::RubyElementType;
