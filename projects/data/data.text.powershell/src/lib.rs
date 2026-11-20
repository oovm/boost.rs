#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod ast;
pub mod builder;

pub mod language;
pub mod lexer;

/// Parser module.
pub mod parser;

pub use crate::{
    ast::PowerShellRoot, builder::PowerShellBuilder, language::PowerShellLanguage, lexer::token_type::PowerShellTokenType,
    parser::PowerShellParser,
};



pub use parser::element_type::PowerShellElementType;
