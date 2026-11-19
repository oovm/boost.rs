use crate::{ast::PrologRoot, language::PrologLanguage, parser::PrologParser};
use data_text_core::{Builder, BuilderCache, OakDiagnostics, TextEdit, parser::Parser, source::Source};

/// Prolog builder.
#[derive(Clone)]
pub struct PrologBuilder {
    config: PrologLanguage,
}

impl PrologBuilder {
    /// Create a new `PrologBuilder`.
    pub fn new(config: PrologLanguage) -> Self {
        Self { config }
    }
}

impl Builder<PrologLanguage> for PrologBuilder {
    fn build<'a, S: Source + ?Sized>(&self, source: &S, edits: &[TextEdit], _cache: &'a mut impl BuilderCache<PrologLanguage>) -> data_text_core::builder::BuildOutput<PrologLanguage> {
        let parser = PrologParser::new(&self.config);
        let mut cache = data_text_core::parser::session::ParseSession::<PrologLanguage>::default();
        let parse_result = parser.parse(source, edits, &mut cache);

        match parse_result.result {
            Ok(_green_tree) => {
                // Placeholder AST building
                OakDiagnostics { result: Ok(PrologRoot {}), diagnostics: parse_result.diagnostics }
            }
            Err(parse_error) => OakDiagnostics { result: Err(parse_error), diagnostics: parse_result.diagnostics },
        }
    }
}
