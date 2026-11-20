#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module for Perl.
pub mod ast;
/// Builder module for Perl.
pub mod builder;
/// Language configuration module for Perl.
pub mod language;
/// Lexer module for Perl.
pub mod lexer;

/// Parser module for Perl.
pub mod parser;

pub use crate::{ast::PerlRoot, builder::PerlBuilder, language::PerlLanguage, lexer::PerlLexer, parser::PerlParser};



/// Perl token type.
pub use lexer::token_type::PerlTokenType;
/// Perl element type.
pub use parser::element_type::PerlElementType;
