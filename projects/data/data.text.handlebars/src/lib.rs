#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod ast;
/// AST Builder
pub mod builder;

mod language;
mod lexer;
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
/// MCP module.
#[cfg(feature = "mcp")]
pub mod mcp;

mod parser;

pub use crate::{
    ast::HandlebarsRoot,
    builder::HandlebarsBuilder,
    language::HandlebarsLanguage,
    lexer::{HandlebarsLexer, token_type::HandlebarsTokenType},
    parser::{HandlebarsParser, element_type::HandlebarsElementType},
};
