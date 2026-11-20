#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![feature(portable_simd)]
#![warn(missing_docs)]
#![doc = include_str!("readme.md")]

/// AST module for HTML nodes.
pub mod ast;
/// Builder module for constructing HTML trees.
pub mod builder;
/// Kind module defining HTML syntax types.
/// Language module for HTML configuration.
pub mod language;
/// Lexer module for HTML tokenization.
pub mod lexer;

/// Parser module for HTML syntax analysis.
pub mod parser;
/// CSS selector subset queries over parsed HTML AST.
pub mod query;

pub use crate::{ast::HtmlDocument, builder::HtmlBuilder, language::HtmlLanguage, lexer::HtmlLexer, parser::HtmlParser};


pub use lexer::token_type::HtmlTokenType;
pub use parser::element_type::HtmlElementType;
