#![doc = include_str!("readme.md")]
#![deny(missing_docs)]

/// AST 打印器:将类型语法树还原为可重新解析的 TypeScript 文本。
pub mod printer;

/// AST module for TypeScript.
pub mod ast;
/// Builder module for TypeScript.
pub mod builder;
/// Language definition for TypeScript.
pub mod language;
/// Lexer for TypeScript.
pub mod lexer;
/// Parser for TypeScript.
pub mod parser;
/// Module-level binding and reference facts for rename-safe bundling.
pub mod scope;

pub use crate::{
    ast::TypeScriptRoot,
    builder::{TypeScriptBuilder, decode_string_literal_text},
    language::TypeScriptLanguage,
    lexer::{TypeScriptLexer, token_type::TypeScriptTokenType},
    parser::{TypeScriptParser, element_type::TypeScriptElementType},
};
