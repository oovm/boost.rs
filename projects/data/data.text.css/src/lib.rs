#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]
#![doc = include_str!("readme.md")]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;
/// Type definitions module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;
/// Parser module.
pub mod parser;

// pub use data_text_core::{TokenType, ElementType};

pub use crate::{
    ast::{AtRule, CssNode, CssRoot, Declaration, RuleSet, Selector, Value},
    builder::CssBuilder,
    language::CssLanguage,
    lexer::{CssLexer, CssTokenType},
    parser::{CssElementType, CssParser},
};
