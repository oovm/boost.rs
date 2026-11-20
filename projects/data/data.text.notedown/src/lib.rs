#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod ast;
pub mod builder;

pub mod language;
pub mod lexer;

/// Parser functionality for Notedown.
pub mod parser;

pub use crate::{
    ast::NoteDocument as NoteRoot,
    builder::NoteBuilder,
    language::NotedownLanguage as NoteLanguage,
    lexer::{NotedownLexer as NoteLexer, token_type::NoteTokenType},
    parser::NoteParser,
};



pub use parser::element_type::NoteElementType;

fn build_output(source: &str) -> data_text_core::builder::BuildOutput<NoteLanguage> {
    use data_text_core::{Builder, parser::session::ParseSession, source::SourceText};
    let config = NoteLanguage::default();
    let builder = NoteBuilder::new(&config);
    let text = SourceText::new(source.to_string());
    let mut cache = ParseSession::default();
    builder.build(&text, &[], &mut cache)
}

/// Collect unified diagnostics from a Notedown parse without rendering to stderr.
#[cfg(feature = "diagnostic")]
pub fn diagnostic_set_from_notedown(source: &str) -> data_text_core::diagnostic::DiagnosticSet {
    data_text_core::diagnostic::diagnostic_set_from_output(&build_output(source))
}
