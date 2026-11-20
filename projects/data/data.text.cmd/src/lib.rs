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
/// Parser module.
pub mod parser;

pub use builder::CmdBuilder as Builder;
pub use language::CmdLanguage as Language;
pub use lexer::{CmdLexer as Lexer, token_type::CmdTokenType as TokenType};
pub use parser::{CmdParser as Parser, element_type::CmdElementType as ElementType};
