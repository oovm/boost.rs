#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;

/// Syntax kind module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;

/// Parser module.
pub mod parser;

pub use crate::{ast::NginxRoot, builder::NginxBuilder, language::NginxLanguage, lexer::NginxLexer, parser::NginxParser};



pub use lexer::token_type::NginxTokenType;
pub use parser::element_type::NginxElementType;
