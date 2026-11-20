#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

extern crate alloc;

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;

/// Type definitions module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;
/// Parser module.
pub mod parser;
pub use crate::{ast::VRoot, builder::VomlBuilder, language::VomlLanguage, lexer::VomlLexer, parser::VomlParser};

/// Parses a Voml string into a `VRoot`.
pub fn parse(voml: &str) -> Result<crate::ast::VRoot, String> {
    use data_text_core::{Builder, parser::session::ParseSession, source::SourceText};
    let language = VomlLanguage::default();
    let builder = VomlBuilder::new(&language);
    let source = SourceText::new(voml.to_string());
    let mut cache = ParseSession::default();
    let result = builder.build(&source, &[], &mut cache);
    result.result.map_err(|e| format!("{:?}", e))
}
/// Syntax module.
pub use data_text_core::{ElementType, TokenType};

pub use lexer::token_type::VomlTokenType;
pub use parser::element_type::VomlElementType;
