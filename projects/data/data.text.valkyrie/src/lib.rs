#![doc = include_str!("readme.md")]
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

/// CST token-gap source formatter.
pub mod formatter;
/// AST pretty printer (`source` → parse → AST → text).
pub mod printer;

pub use crate::{
    ast::ValkyrieRoot,
    builder::ValkyrieBuilder,
    language::ValkyrieLanguage,
    lexer::{ValkyrieLexer, token_type::ValkyrieTokenType},
    parser::{ValkyrieParser, element_type::ValkyrieElementType},
};
pub use data_text_core::{ElementType, TokenType};
