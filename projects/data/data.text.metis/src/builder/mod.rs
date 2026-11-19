use data_text_core::{
    Builder,
    source::{Source, TextEdit},
};

use crate::language::MetisLanguage;

/// Builder for Metis island-language CST.
pub struct MetisBuilder;

impl Builder<MetisLanguage> for MetisBuilder {
    fn build<'a, S: Source + ?Sized>(&self, text: &S, edits: &[TextEdit], cache: &'a mut impl data_text_core::builder::BuilderCache<MetisLanguage>) -> data_text_core::builder::BuildOutput<MetisLanguage> {
        use data_text_core::parser::Parser;

        let language = MetisLanguage::default();
        let parser = language.parser();
        let green_tree = parser.parse(text, edits, cache);
        data_text_core::errors::OakDiagnostics { result: Err(data_text_core::errors::OakError::custom_error("Metis CST builder not implemented yet")), diagnostics: green_tree.diagnostics }
    }
}

impl Default for MetisBuilder {
    fn default() -> Self {
        Self
    }
}
