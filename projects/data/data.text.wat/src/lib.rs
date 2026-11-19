#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;
// pub mod formatter;
//
// pub mod highlighter;
/// Type definitions module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;
// /// LSP module.
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
// pub mod mcp;
/// Parser module.
pub mod parser;

pub use crate::{ast::WatRoot, builder::WatBuilder, language::WatLanguage, lexer::WatLexer, parser::WatParser};

//
/// Dummy highlighter for Wat.
#[cfg(feature = "language-highlight")]
pub mod dummy_highlighter {}

// /// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::WatLanguageService;

pub use lexer::token_type::WatTokenType;
pub use parser::element_type::WatElementType;
