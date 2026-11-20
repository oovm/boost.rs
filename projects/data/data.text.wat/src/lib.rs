#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;
// pub mod formatter;
//
// pub mod highlighter;
/// Type definitions module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;
// /// LSP module.
// pub mod mcp;
/// Parser module.
pub mod parser;

pub use crate::{ast::WatRoot, builder::WatBuilder, language::WatLanguage, lexer::WatLexer, parser::WatParser};

//
/// Dummy highlighter for Wat.
pub mod dummy_highlighter {}

// /// LSP implementation.

pub use lexer::token_type::WatTokenType;
pub use parser::element_type::WatElementType;
