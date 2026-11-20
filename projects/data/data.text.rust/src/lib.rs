#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![feature(portable_simd)]
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

pub use crate::{ast::RustRoot, builder::RustBuilder, language::RustLanguage, lexer::RustLexer, parser::RustParser};
pub use lexer::token_type::RustTokenType;
pub use parser::RustElementType;


