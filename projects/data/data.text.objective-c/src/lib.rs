#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;

/// Language definitions.
pub mod language;
/// Lexer module.
pub mod lexer;

/// Parser module.
pub mod parser;

pub use crate::{
    ast::ObjectiveCRoot, builder::ObjectiveCBuilder, language::ObjectiveCLanguage, lexer::ObjectiveCLexer, parser::ObjectiveCParser,
};



// #[cfg(feature = "mcp")]
// pub use crate::mcp::serve_objective_c_mcp;
pub use lexer::token_type::ObjectiveCTokenType;
pub use parser::element_type::ObjectiveCElementType;
