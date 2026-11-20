#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;
/// Kind definition module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;
/// Parser module.
pub mod parser;

// MCP module.
// #[cfg(feature = "mcp")]
// pub mod mcp;

pub use crate::{ast::DRoot, builder::DBuilder, language::DLanguage, lexer::DLexer, parser::DParser};



// MCP service implementation.
// #[cfg(feature = "mcp")]
// pub use crate::mcp::serve_d_mcp;
pub use lexer::token_type::DTokenType;
pub use parser::element_type::DElementType;
