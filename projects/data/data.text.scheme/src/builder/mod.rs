#![doc = include_str!("readme.md")]

use crate::{language::SchemeLanguage, parser::SchemeParser};
use data_text_core::{Builder, BuilderCache, Lexer, Parser, TextDiagnostics, TextEdit, source::Source};

/// Builder for the Scheme language.
#[derive(Clone)]
pub struct SchemeBuilder<'config> {
    config: &'config SchemeLanguage,
}

impl<'config> SchemeBuilder<'config> {
    /// Creates a new `SchemeBuilder` with the given configuration.
    pub fn new(config: &'config SchemeLanguage) -> Self {
        Self { config }
    }
}

impl<'config> Builder<SchemeLanguage> for SchemeBuilder<'config> {
    fn build<'a, S: Source + ?Sized>(
        &self,
        source: &S,
        edits: &[TextEdit],
        cache: &'a mut impl BuilderCache<SchemeLanguage>,
    ) -> TextDiagnostics<()> {
        let parser = SchemeParser::new(self.config);
        let lexer = crate::lexer::SchemeLexer::new(&self.config);

        lexer.lex(source, edits, cache);
        let parse_result = parser.parse(source, edits, cache);

        match parse_result.result {
            Ok(_) => TextDiagnostics { result: Ok(()), diagnostics: parse_result.diagnostics },
            Err(e) => TextDiagnostics { result: Err(e), diagnostics: parse_result.diagnostics },
        }
    }
}
