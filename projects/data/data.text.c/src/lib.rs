#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;
/// Type definition module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;
#[cfg(feature = "lsp")]
pub mod lsp;
/// Parser module.
pub mod parser;

pub use data_text_core::{ElementType, Language, LanguageCategory, TokenType};
pub use language::CLanguage;
pub use lexer::{CLexer, token_type::CTokenType};
pub use parser::{CParser, element_type::CElementType};
