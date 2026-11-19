#![doc = include_str!("readme.md")]
use crate::language::OrgModeLanguage;
use data_text_core::{Builder, BuilderCache, TextDiagnostics, TextEdit, source::Source};

/// Org-mode AST builder.
pub struct OrgModeBuilder {}

impl OrgModeBuilder {
    /// Creates a new `OrgModeBuilder`.
    /// Creates a new OrgModeBuilder with the given language configuration.
    pub fn new(config: &OrgModeLanguage) -> Self {
        Self {}
    }
}

impl Builder<OrgModeLanguage> for OrgModeBuilder {
    fn build<'a, S: Source + ?Sized>(
        &self,
        _source: &S,
        _edits: &[TextEdit],
        _cache: &'a mut impl BuilderCache<OrgModeLanguage>,
    ) -> TextDiagnostics<()> {
        TextDiagnostics { result: Ok(()), diagnostics: Vec::new() }
    }
}
