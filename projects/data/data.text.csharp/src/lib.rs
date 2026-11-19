#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]
#![doc(html_logo_url = "https://raw.githubusercontent.com/ygg-lang/oaks/refs/heads/dev/documents/logo.svg")]
#![doc(html_favicon_url = "https://raw.githubusercontent.com/ygg-lang/oaks/refs/heads/dev/documents/logo.svg")]

use data_text_core::Builder;

/// AST module containing high-level C# syntax tree definitions.
pub mod ast;
/// Builder module for converting green trees into high-level AST nodes.
pub mod builder;
/// Language definition and configuration for C#.
pub mod language;
/// Lexer implementation for C#.
pub mod lexer;
/// LSP-related functionality (hover, completion, highlighting) for C#.
#[cfg(any(feature = "lsp", feature = "oak-highlight", feature = "oak-pretty-print"))]
pub mod lsp;
/// Parser implementation for C#.
pub mod parser;

pub use ast::CSharpRoot;
pub use builder::CSharpBuilder;
pub use language::CSharpLanguage;
pub use lexer::{CSharpLexer, token_type::CSharpTokenType};
pub use parser::CSharpParser;

/// Parses C# source code into a [CSharpRoot] AST.
///
/// This is a convenience function that initializes the language, builder,
/// and parser to process the source text.
///
/// # Errors
///
/// Returns an [data_text_core::TextError] if parsing fails.
pub fn parse(source: &str) -> Result<CSharpRoot, data_text_core::TextError> {
    let language = CSharpLanguage::new();
    let builder = CSharpBuilder::new(&language);
    let source_text = data_text_core::source::SourceText::new(source);
    let mut session = data_text_core::parser::ParseSession::<CSharpLanguage>::default();
    let output = builder.build(&source_text, &[], &mut session);
    output.result
}

pub use parser::element_type::CSharpElementType;

/// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::CSharpLanguageService;
