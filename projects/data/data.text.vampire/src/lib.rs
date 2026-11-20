#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;

/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;

/// Parser module.
pub mod parser;

/// Core exports for the Vampire language.
pub use crate::{ast::VampireRoot, language::VampireLanguage, lexer::VampireLexer, parser::VampireParser};

/// Core trait exports from data_text_core.
pub use data_text_core::{ElementType, TokenType};



/// Token type enumeration for Vampire lexer.
pub use lexer::token_type::VampireTokenType;
/// Element type enumeration for Vampire parser.
pub use parser::element_type::VampireElementType;
