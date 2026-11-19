#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]
#![allow(missing_copy_implementations)]

/// The AsciiDoc language implementation for Oaks.
pub mod language;

/// Abstract Syntax Tree for AsciiDoc.
pub mod ast;

/// Lexer for tokenizing AsciiDoc source.
pub mod lexer;

/// Parser for building AsciiDoc syntax trees.
pub mod parser;

pub use crate::{ast::AsciidocRoot, language::AsciidocLanguage, lexer::AsciidocLexer, parser::AsciidocParser};
