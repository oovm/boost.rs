#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

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
/// Syntax module.
pub use crate::ast::ScssRoot;
/// Re-exports of core SCSS types.
pub use crate::{builder::ScssBuilder, language::ScssLanguage, lexer::ScssLexer, parser::ScssParser};



/// Token type for SCSS lexing.
/// Re-export of lexer types.
pub use lexer::token_type::ScssTokenType;
/// Element type for SCSS parsing.
pub use parser::element_type::ScssElementType;
