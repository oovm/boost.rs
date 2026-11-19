use crate::{ast::OCamlRoot, language::OCamlLanguage, parser::OCamlParser};
use data_text_core::{Builder, BuilderCache, Lexer, Parser, TextDiagnostics, TextEdit, source::Source};

/// A builder for OCaml.
#[derive(Clone)]
pub struct OCamlBuilder<'config> {
    /// The language configuration.
    pub config: &'config OCamlLanguage,
}

impl<'config> OCamlBuilder<'config> {
    /// Creates a new OCaml builder.
    pub fn new(config: &'config OCamlLanguage) -> Self {
        Self { config }
    }
}

impl<'config> Builder<OCamlLanguage> for OCamlBuilder<'config> {
    fn build<'a, S: Source + ?Sized>(
        &self,
        source: &'a S,
        edits: &[TextEdit],
        _cache: &'a mut impl BuilderCache<OCamlLanguage>,
    ) -> TextDiagnostics<OCamlRoot> {
        let parser = OCamlParser::new(self.config);
        let lexer = crate::lexer::OCamlLexer::new(self.config);

        let mut session = data_text_core::parser::session::ParseSession::<OCamlLanguage>::default();
        lexer.lex(source, edits, &mut session);
        let parse_result = parser.parse(source, edits, &mut session);

        match parse_result.result {
            Ok(_green_tree) => {
                let ast_root = OCamlRoot { items: vec![] };
                TextDiagnostics { result: Ok(ast_root), diagnostics: parse_result.diagnostics }
            }
            Err(parse_error) => TextDiagnostics { result: Err(parse_error), diagnostics: parse_result.diagnostics },
        }
    }
}
