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

pub use crate::{ast::HlslRoot, builder::HlslBuilder, language::HlslLanguage, lexer::HlslLexer, parser::HlslParser};

/// Parses HLSL source into the typed AST.
///
/// Syntax errors are returned as `Err` with the diagnostic message. Callers
/// that need the full diagnostics should use [`HlslBuilder`] directly.
pub fn parse_hlsl(source: &str) -> Result<HlslRoot, String> {
    use data_text_core::{Builder, parser::session::ParseSession, source::SourceText};

    let language = HlslLanguage::default();
    let builder = HlslBuilder::new(&language);
    let text = SourceText::new(source.to_string());
    let mut cache = ParseSession::<HlslLanguage>::default();
    builder.build(&text, &[], &mut cache).result.map_err(|error| error.to_string())
}


pub use lexer::token_type::HlslTokenType;
pub use parser::element_type::HlslElementType;
