use crate::{ast::DjangoRoot, language::DjangoLanguage, parser::DjangoParser};
use data_text_core::{Builder, BuilderCache, GreenNode, Parser, SourceText, TextDiagnostics, TextEdit, TextError, source::Source};

pub struct DjangoBuilder<'config> {
    config: &'config DjangoLanguage,
}

impl<'config> DjangoBuilder<'config> {
    pub fn new(config: &'config DjangoLanguage) -> Self {
        Self { config }
    }
}

impl<'config> Builder<DjangoLanguage> for DjangoBuilder<'config> {
    type Root = DjangoRoot;

    fn build<'a, S: Source + ?Sized, BC: BuilderCache<'a, DjangoLanguage>>(
        &self,
        source: &'a S,
        _arena: &'a data_text_core::SyntaxArena,
        edits: &[TextEdit],
        _cache: BC,
    ) -> TextDiagnostics<DjangoRoot> {
        let parser = DjangoParser::new(self.config);
        let mut cache = data_text_core::parser::session::ParseHeap::<DjangoLanguage>::default();
        let parse_result = parser.parse(source, edits, &mut cache);

        match parse_result.result {
            Ok(green_tree) => {
                let source_text = SourceText::new(source.get_text_in((0..source.length()).into()).into_owned());
                match self.build_root(green_tree, &source_text) {
                    Ok(ast_root) => TextDiagnostics { result: Ok(ast_root), diagnostics: parse_result.diagnostics },
                    Err(e) => TextDiagnostics { result: Err(e), diagnostics: parse_result.diagnostics },
                }
            }
            Err(e) => TextDiagnostics { result: Err(e), diagnostics: parse_result.diagnostics },
        }
    }
}

impl<'config> DjangoBuilder<'config> {
    fn build_root(&self, _green_tree: GreenNode<DjangoLanguage>, _source: &SourceText) -> Result<DjangoRoot, TextError> {
        // TODO: Implement AST building
        Err(TextError::Other("Not implemented".to_string()))
    }
}
