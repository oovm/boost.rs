#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;
mod language;
mod lexer;
/// Parser module.
pub mod parser;
// pub mod lsp;

pub use builder::ClojureBuilder;
pub use language::ClojureLanguage;
pub use lexer::{ClojureLexer, token_type::ClojureTokenType};
pub use parser::{ClojureParser, element_type::ClojureElementType};
