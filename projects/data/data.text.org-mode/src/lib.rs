#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
pub mod builder;

pub mod language;
pub mod lexer;

pub mod parser;

pub use crate::{ast::OrgModeRoot, builder::OrgModeBuilder, language::OrgModeLanguage, lexer::OrgModeLexer, parser::OrgModeParser};



pub use lexer::token_type::OrgModeTokenType;
pub use parser::element_type::OrgModeElementType;
