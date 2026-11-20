#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;
/// Syntax kind module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;

/// Parser module.
pub mod parser;

pub use crate::{
    ast::TomlRoot,
    builder::TomlBuilder,
    language::TomlLanguage,
    lexer::{TomlLexer, token_type::TomlTokenKind as TomlSyntaxKind},
    parser::{TomlParser, parse, parse_with_config},
};
pub use data_text_core::{Builder, TokenType};


/// Serializes a Rust type into a TOML string.
#[cfg(feature = "serde")]
pub use crate::language::to_string;

/// Deserializes a TOML string into a Rust type.
#[cfg(feature = "serde")]
pub use crate::language::from_str;
pub use crate::parser::element_type::TomlElementType as ElementType;
