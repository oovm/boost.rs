#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// Abstract Syntax Tree (AST) definitions for Haskell.
pub mod ast;
/// Incremental tree builder for Haskell.
pub mod builder;

pub mod language;
pub mod lexer;

pub mod parser;

pub use crate::{ast::HaskellRoot, builder::HaskellBuilder, language::HaskellLanguage, lexer::HaskellLexer, parser::HaskellParser};



/// Haskell token types.
pub use lexer::token_type::{HaskellToken, HaskellTokenType};
/// Haskell element types.
pub use parser::element_type::HaskellElementType;
