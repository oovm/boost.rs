#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;
/// Kind definition module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;

/// Parser module.
pub mod parser;

pub use crate::{builder::YamlBuilder, language::YamlLanguage, lexer::YamlLexer, parser::YamlParser};


/// Parses a YAML string.
pub fn parse(yaml: &str) -> Result<crate::ast::YamlRoot, String> {
    use data_text_core::{Builder, parser::session::ParseSession, source::SourceText};
    let language = YamlLanguage::new();
    let builder = YamlBuilder::new(&language);
    let source = SourceText::new(yaml.to_string());
    let mut cache = ParseSession::default();
    let result = builder.build(&source, &[], &mut cache);
    result.result.map_err(|e| format!("{:?}", e))
}

/// Deserializes a YAML string into a Rust type.
#[cfg(feature = "serde")]
pub use crate::language::from_str;

/// Serializes a Rust type into a YAML string.
#[cfg(feature = "serde")]
pub use crate::language::to_string;


pub use lexer::token_type::YamlTokenType;
pub use parser::element_type::YamlElementType;
