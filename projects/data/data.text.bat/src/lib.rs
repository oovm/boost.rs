#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
/// LSP module.
pub mod lsp;
/// Parser module.
pub mod parser;

pub use builder::BatBuilder as Builder;
pub use language::BatLanguage as Language;
pub use lexer::{BatLexer as Lexer, token_type::BatTokenType as TokenType};
pub use parser::{BatParser as Parser, element_type::BatElementType as ElementType};
