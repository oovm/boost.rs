#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod ast;
pub mod builder;

pub mod language;
pub mod lexer;
pub mod parser;

pub use crate::{
    ast::PurescriptRoot, builder::PurescriptBuilder, language::PurescriptLanguage, lexer::PurescriptLexer, parser::PurescriptParser,
};


/// MCP service implementation.
// #[cfg(feature = "mcp")]
// pub mod mcp;
// #[cfg(feature = "mcp")]
// pub use crate::mcp::serve_purescript_mcp;
/// Re-export of parser types.
pub use parser::element_type::PurescriptElementType;
