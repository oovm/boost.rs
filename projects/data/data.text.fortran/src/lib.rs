#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;
/// Language module.
mod language;
/// Lexer module.
mod lexer;
/// LSP module.
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
/// Parser module.
pub mod parser;

pub use crate::{
    ast::FortranRoot,
    builder::FortranBuilder,
    language::FortranLanguage,
    lexer::{FortranLexer, token_type::FortranTokenType},
    parser::element_type::FortranElementType,
};
