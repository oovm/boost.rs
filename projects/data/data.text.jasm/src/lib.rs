#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;

/// Type definitions module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;

/// Parser module.
pub mod parser;
/// Syntax module.
pub use crate::{ast::JasmRoot, builder::JasmBuilder, language::JasmLanguage, lexer::JasmLexer, parser::JasmParser};



/// Jasm token type.
pub use lexer::token_type::JasmTokenType;
/// Jasm element type.
pub use parser::element_type::JasmElementType;
