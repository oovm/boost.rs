#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

extern crate alloc;

/// AST module.
pub mod ast;
/// Builder module for constructing V language AST from syntax trees.
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

pub use crate::{ast::*, builder::VLangBuilder, language::VLangLanguage, lexer::VLangLexer, parser::VLangParser};
pub use data_text_core::{ElementType, TokenType};

// #[cfg(feature = "language-highlight")]
// pub use crate::lsp::highlighter::VLangHighlighter;

// /// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::VLangLanguageService;
// #[cfg(feature = "lsp")]
// pub use crate::lsp::formatter::VLangFormatter;
pub use lexer::token_type::VLangTokenType;
pub use parser::element_type::VLangElementType;
