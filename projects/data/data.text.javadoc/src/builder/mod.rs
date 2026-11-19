#![doc = include_str!("readme.md")]
use crate::{ast::JavadocRoot, language::JavadocLanguage};
use data_text_core::builder::Builder;

/// Javadoc syntax tree builder
pub struct JavadocBuilder {
    /// Javadoc language configuration
    pub config: JavadocLanguage,
}

impl Default for JavadocBuilder {
    fn default() -> Self {
        Self { config: JavadocLanguage::default() }
    }
}

impl Builder<JavadocLanguage> for JavadocBuilder {
    fn build<'a, S: data_text_core::source::Source + ?Sized>(&self, _source: &S, _edits: &[data_text_core::source::TextEdit], _cache: &'a mut impl data_text_core::builder::BuilderCache<JavadocLanguage>) -> data_text_core::builder::BuildOutput<JavadocLanguage> {
        data_text_core::errors::OakDiagnostics { result: Ok(JavadocRoot { description: vec![], tags: vec![] }), diagnostics: vec![] }
    }
}
