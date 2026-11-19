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

/// Fluent translator.
pub mod translator;

pub use crate::{
    ast::FluentRoot,
    builder::FluentBuilder,
    language::FluentLanguage,
    lexer::{FluentLexer, token_type::FluentTokenKind as FluentSyntaxKind},
    parser::{FluentParser, parse, parse_with_config},
    translator::Translator,
};

pub use data_text_core::{Builder, TokenType};

/// Deserializes a Fluent string into a Fluent AST.
pub use crate::language::from_str;

/// Serializes a Fluent AST into a Fluent string.
pub use crate::language::to_string;
