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

pub use crate::{ast::SwiftRoot, builder::SwiftBuilder, language::SwiftLanguage, lexer::SwiftLexer, parser::SwiftParser};



pub use crate::{lexer::token_type::SwiftTokenType as TokenType, parser::element_type::SwiftElementType as ElementType};
