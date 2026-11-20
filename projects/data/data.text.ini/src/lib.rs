#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

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

pub use crate::{
    ast::IniRoot,
    builder::IniBuilder,
    language::{IniLanguage, IniValueStyle},
    lexer::IniLexer,
    parser::IniParser,
};

/// Parses an INI string with the default dialect.
pub fn parse(ini: &str) -> Result<crate::ast::IniRoot, String> {
    parse_with(ini, &IniLanguage::default())
}

/// Parses an INI string with an explicit dialect configuration.
pub fn parse_with(ini: &str, language: &IniLanguage) -> Result<crate::ast::IniRoot, String> {
    use data_text_core::{Builder, parser::session::ParseSession, source::SourceText};
    let builder = IniBuilder::new(language);
    let source = SourceText::new(ini.to_string());
    let mut cache = ParseSession::default();
    let result = builder.build(&source, &[], &mut cache);
    result.result.map_err(|e| format!("{:?}", e))
}



pub use lexer::token_type::IniTokenType;
pub use parser::element_type::IniElementType;
