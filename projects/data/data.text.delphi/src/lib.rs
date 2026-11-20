#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod ast;
mod builder;

pub mod language;
pub mod lexer;
/// Delphi parser implementation.
pub mod parser;

// Re-export main types
pub use crate::{
    ast::DelphiRoot,
    builder::DelphiBuilder,
    language::DelphiLanguage,
    lexer::{DelphiLexer, token_type::DelphiTokenType},
    parser::element_type::DelphiElementType,
};

