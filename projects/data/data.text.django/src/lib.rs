#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod ast;
mod language;
mod lexer;
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
mod parser;

#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::DjangoHighlighter;
pub use crate::{
    ast::DjangoRoot,
    language::DjangoLanguage,
    lexer::{DjangoLexer, token_type::DjangoTokenType},
    parser::{DjangoParser, element_type::DjangoElementType},
};
