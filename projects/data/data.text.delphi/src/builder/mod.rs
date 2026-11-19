use crate::{ast::*, language::DelphiLanguage, parser::DelphiParser};
use data_text_core::{Builder, BuilderCache, GreenNode, TextDiagnostics, TextError, Parser, RedNode, SourceText, TextEdit, source::Source};

/// AST builder for the Delphi language
#[derive(Clone)]
pub struct DelphiBuilder {
    config: DelphiLanguage,
}

impl DelphiBuilder {
    /// Creates a new `DelphiBuilder`
    pub fn new(config: DelphiLanguage) -> Self {
        Self { config }
    }
}

impl Builder<DelphiLanguage> for DelphiBuilder {
    fn build<'a, S: Source + ?Sized>(&self, source: &'a S, edits: &[TextEdit], _cache: &'a mut impl BuilderCache<DelphiLanguage>) -> data_text_core::builder::BuildOutput<DelphiLanguage> {
        let parser = DelphiParser::new(&self.config);

        let mut parse_cache = data_text_core::parser::session::ParseSession::<DelphiLanguage>::default();
        let parse_result = parser.parse(source, edits, &mut parse_cache);

        match parse_result.result {
            Ok(green_tree) => {
                let source_text = SourceText::new(source.get_text_in((0..source.length()).into()).into_owned());
                match self.build_root(green_tree, &source_text) {
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

impl DelphiBuilder {
    fn build_root<'a>(&self, green_tree: &'a GreenNode<'a, DelphiLanguage>, _source: &SourceText) -> Result<DelphiRoot, TextError> {
        let red_root = RedNode::new(green_tree, 0);
        // Simple implementation
        Ok(DelphiRoot { items: Vec::new(), span: red_root.span() })
    }
}
