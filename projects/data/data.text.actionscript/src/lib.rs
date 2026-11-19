#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// Abstract Syntax Tree (AST) definitions for ActionScript.
pub mod ast;
mod builder;

mod language;
/// Lexer for ActionScript.
pub mod lexer;
/// Language Server Protocol (LSP) and editor integration for ActionScript.
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;

/// Parser for ActionScript.
pub mod parser;

#[cfg(feature = "lsp")]
#[cfg(feature = "mcp")]
/// Model Context Protocol (MCP) support for ActionScript.
pub mod mcp;

// Re-export main types
pub use crate::{
    ast::ActionScriptRoot,
    builder::ActionScriptBuilder,
    language::ActionScriptLanguage,
    lexer::{ActionScriptLexer, ActionScriptTokenType},
    parser::{ActionScriptElementType, ActionScriptParser},
};
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub use crate::lsp::formatter::ActionScriptFormatter;

#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::ActionScriptHighlighter;

#[cfg(feature = "lsp")]
pub use crate::lsp::ActionScriptLanguageService;

#[cfg(feature = "mcp")]
pub use crate::mcp::serve_actionscript_mcp;