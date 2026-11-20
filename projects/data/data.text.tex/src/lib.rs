#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module containing TeX syntax tree definitions.
pub mod ast;
/// Builder module for constructing TeX ASTs.
pub mod builder;

/// Language definition and configuration for TeX.
pub mod language;
/// Lexer implementation for TeX.
pub mod lexer;

/// Parser implementation for TeX.
pub mod parser;

pub use crate::{ast::TexRoot, builder::TexBuilder, language::TexLanguage, lexer::TexLexer, parser::TexParser};



pub use lexer::token_type::TexTokenType;
pub use parser::element_type::TexElementType;
