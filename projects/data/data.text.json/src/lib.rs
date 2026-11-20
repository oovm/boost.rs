#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![feature(portable_simd)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;

// pub mod formatter;

/// Type definitions module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;

/// Parser module.
pub mod parser;

pub use crate::{
    ast::{JsonRoot, JsonValueNode},
    builder::JsonBuilder,
    language::JsonLanguage,
    lexer::JsonLexer,
    parser::JsonParser,
};


#[cfg(feature = "serde")]
pub use crate::language::to_string;

#[cfg(feature = "serde")]
pub use crate::language::from_str;

/// Parses a JSON string into a `JsonValueNode` AST.
pub fn parse(json: &str) -> Result<crate::ast::JsonValueNode, String> {
    use data_text_core::{Builder, parser::session::ParseSession, source::SourceText};
    let language = JsonLanguage::default();
    let builder = JsonBuilder::new(&language);
    let source = SourceText::new(json.to_string());
    let mut cache = ParseSession::default();
    let result = builder.build(&source, &[], &mut cache);
    result.result.map(|root| root.value).map_err(|e| format!("{:?}", e))
}

pub use language_macro::json;

// pretty-print is always available through `data-text-pretty`
// pub use crate::lsp::formatter::JsonFormatter;

pub use lexer::token_type::JsonTokenType;
pub use parser::element_type::JsonElementType;
