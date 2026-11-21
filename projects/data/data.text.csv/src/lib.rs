#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]
//! Csv support for the `data.text` language framework.

// AST module.
// pub mod ast;
// Builder module.
// pub mod builder;

/// Kind definition module.
/// Language configuration module.
pub mod language;
pub use language::CsvLanguage;
/// Lexer module.
pub mod lexer;
pub use lexer::CsvLexer;
/// Parser module.
pub mod parser;
/// Plain record parsing and escaping shared by spreadsheet products.
pub mod table;

pub use crate::parser::CsvParser;
pub use table::{Field, escape_field, parse_records, write_records};
pub use data_text_core::{ElementType, TokenType};
