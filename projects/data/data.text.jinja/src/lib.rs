#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module
pub mod ast;
/// Builder module
pub mod builder;
pub mod language;
mod lexer;
/// Parser module
pub mod parser;

pub use ast::{JinjaElement, JinjaRoot};
pub use builder::JinjaBuilder;
pub use language::JinjaLanguage;
pub use lexer::{JinjaLexer, token_type::JinjaTokenType};
pub use parser::{JinjaParser, element_type::JinjaElementType};


