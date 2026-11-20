#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST definitions for AsciiDoc.
pub mod ast;
/// Builder for the AsciiDoc AST.
pub mod builder;
pub mod language;
/// Lexer for AsciiDoc.
pub mod lexer;
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
/// Parser for AsciiDoc.
pub mod parser;

pub use crate::{
    ast::AsciiDocRoot,
    builder::AsciiDocBuilder,
    language::AsciiDocLanguage,
    lexer::{AsciiDocLexer, token_type::AsciiDocTokenType},
    parser::AsciiDocParser,
};
pub use parser::element_type::AsciiDocElementType;
