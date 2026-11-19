#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module
pub mod ast;
/// Builder module
pub mod builder;
mod language;
mod lexer;
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
/// LSP module
pub mod lsp;
/// Parser module
pub mod parser;

pub use ast::{LiquidNode, LiquidRoot};
pub use builder::LiquidBuilder;
pub use language::LiquidLanguage;
pub use lexer::{LiquidLexer, token_type::LiquidTokenType};
pub use parser::{LiquidParser, element_type::LiquidElementType};

/// Highlighter implementation
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::LiquidHighlighter;

#[cfg(feature = "lsp")]
pub use crate::lsp::LiquidLanguageService;
/// LSP implementation
#[cfg(feature = "lsp")]
pub use crate::lsp::formatter::LiquidFormatter;
