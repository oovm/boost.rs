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

pub use crate::{ast::DejavuRoot, language::DejavuLanguage, lexer::DejavuLexer, parser::DejavuParser};

pub use crate::builder::DejavuBuilder;

pub use data_text_core::{ElementType, TokenType};



pub use lexer::token_type::DejavuTokenType;
pub use parser::element_type::DejavuElementType;

/// Parses a DejaVu template string into a `DejavuRoot` AST.
pub fn parse(source: &str) -> Result<crate::ast::DejavuRoot, String> {
    use data_text_core::{Builder, ParseSession, SourceText};
    let language = DejavuLanguage::default();
    let builder = DejavuBuilder::new(&language);
    let source_text = SourceText::new(source.to_string());
    let mut cache = ParseSession::default();
    let result = builder.build(&source_text, &[], &mut cache);
    result.result.map_err(|e| format!("{:?}", e))
}
