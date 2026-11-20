#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// Abstract Syntax Tree (AST) definitions for ActionScript.
pub mod ast;
mod builder;

mod language;
/// Lexer for ActionScript.
pub mod lexer;

/// Parser for ActionScript.
pub mod parser;

#[cfg(feature = "lsp")]
#[cfg(feature = "mcp")]

// Re-export main types
pub use crate::{
    ast::ActionScriptRoot,
    builder::ActionScriptBuilder,
    language::ActionScriptLanguage,
    lexer::{ActionScriptLexer, ActionScriptTokenType},
    parser::{ActionScriptElementType, ActionScriptParser},
};



