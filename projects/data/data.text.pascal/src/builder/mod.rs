#![doc = include_str!("readme.md")]
use crate::language::PascalLanguage;
use data_text_core::{Builder, BuilderCache, TextEdit, source::Source};

/// A builder for Pascal source files.
pub struct PascalBuilder;

impl PascalBuilder {
    /// Creates a new `PascalBuilder` with the given language configuration.
    pub fn new(_language: &PascalLanguage) -> Self {
        Self
    }
}

impl Builder<PascalLanguage> for PascalBuilder {
    fn build<'s, S: Source + ?Sized>(
        &self,
        _text: &S,
        _edits: &[TextEdit],
        _cache: &'s mut impl BuilderCache<PascalLanguage>,
    ) -> data_text_core::builder::BuildOutput<PascalLanguage> {
        data_text_core::errors::TextDiagnostics {
            result: Err(data_text_core::errors::TextError::custom_error("Not implemented")),
            diagnostics: vec![],
        }
    }
}
