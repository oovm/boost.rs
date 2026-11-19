#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]
#![allow(missing_copy_implementations)]

/// The reStructuredText language implementation for Oaks.
pub mod language;

/// Abstract Syntax Tree for reStructuredText.
pub mod ast;

/// Lexer for tokenizing reStructuredText source.
pub mod lexer;

/// Parser for building reStructuredText syntax trees.
pub mod parser;

pub use crate::{ast::RstRoot, language::RstLanguage, lexer::RstLexer, parser::RstParser};
