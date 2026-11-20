#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]
#![doc = include_str!("readme.md")]

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

pub use crate::{ast::ZigRoot, builder::ZigBuilder, language::ZigLanguage, lexer::ZigLexer, parser::ZigParser};
pub use data_text_core::{ElementType, TokenType};


// /// MCP service implementation.
// #[cfg(feature = "mcp")]
// pub use crate::mcp::serve_zig_mcp;
pub use lexer::token_type::ZigTokenType;
pub use parser::element_type::ZigElementType;
