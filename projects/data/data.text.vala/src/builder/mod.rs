use crate::{ast::*, language::ValaLanguage, parser::ValaParser};
use data_text_core::{Builder, BuilderCache, GreenNode, Parser, SourceText, TextDiagnostics, TextEdit, source::Source};

/// AST builder for Vala language.
pub struct ValaBuilder<'config> {
    config: &'config ValaLanguage,
}

impl<'config> ValaBuilder<'config> {
    /// Creates a new `ValaBuilder` with the given language configuration.
    pub fn new(config: &'config ValaLanguage) -> Self {
        Self { config }
    }
}

impl<'config> Builder<ValaLanguage> for ValaBuilder<'config> {
    fn build<'a, S: Source + ?Sized>(
        &self,
        source: &'a S,
        edits: &[TextEdit],
        _cache: &'a mut impl BuilderCache<ValaLanguage>,
    ) -> data_text_core::builder::BuildOutput<ValaLanguage> {
        let parser = ValaParser::new(self.config);
        let mut cache = data_text_core::parser::ParseSession::<ValaLanguage>::default();
        let parse_result = parser.parse(source, edits, &mut cache);

        match parse_result.result {
            Ok(green_tree) => {
                let source_text = SourceText::new(source.get_text_in((0..source.length()).into()).into_owned());
                match self.build_root(&green_tree, &source_text) {
                    Ok(ast_root) => TextDiagnostics { result: Ok(ast_root), diagnostics: parse_result.diagnostics },
                    Err(build_error) => {
                        let mut diagnostics = parse_result.diagnostics;
                        diagnostics.push(build_error.clone());
                        TextDiagnostics { result: Err(build_error), diagnostics }
                    }
                }
            }
            Err(parse_error) => TextDiagnostics { result: Err(parse_error), diagnostics: parse_result.diagnostics },
        }
    }
}

impl<'config> ValaBuilder<'config> {
    /// Builds the root AST node from the syntax tree.
    pub fn build_root(&self, _green: &GreenNode<ValaLanguage>, _source: &SourceText) -> Result<ValaRoot, data_text_core::TextError> {
        // Simplified AST building logic
        Ok(ValaRoot { span: (0.._source.len()).into(), items: vec![] })
    }
}
