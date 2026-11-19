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
/// LSP module.
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
/// MCP module.
#[cfg(feature = "mcp")]
pub mod mcp;
/// Parser module.
pub mod parser;

// pub use data_text_core::{TokenType, ElementType};

pub use crate::{
    ast::{AtRule, Declaration, LessNode, LessRoot, RuleSet, Selector, Value},
    builder::LessBuilder,
    language::LessLanguage,
    lexer::{LessLexer, LessTokenType},
    parser::{LessElementType, LessParser},
};
