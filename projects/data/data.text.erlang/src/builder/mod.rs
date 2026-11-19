#![doc = include_str!("readme.md")]
use crate::{ast::*, language::ErlangLanguage, parser::ErlangParser};
use data_text_core::{Builder, BuilderCache, GreenNode, TextDiagnostics, Parser, SourceText, TextEdit, source::Source};

/// AST builder for the Erlang language
#[derive(Clone)]
pub struct ErlangBuilder<'config> {
    /// Language configuration
    config: &'config ErlangLanguage,
}

impl<'config> ErlangBuilder<'config> {
    /// Creates a new Erlang builder
    pub fn new(config: &'config ErlangLanguage) -> Self {
        Self { config }
    }

    /// Builds the AST root node from the syntax tree
    pub fn build_root(&self, _green: &GreenNode<ErlangLanguage>, _source: &SourceText) -> Result<ErlangRoot, data_text_core::TextError> {
        // Simplified AST construction logic
        Ok(ErlangRoot { items: vec![] })
    }
}

impl<'config> Builder<ErlangLanguage> for ErlangBuilder<'config> {
    fn build<'a, S: Source + ?Sized>(&self, source: &'a S, edits: &[TextEdit], _cache: &'a mut impl BuilderCache<ErlangLanguage>) -> data_text_core::builder::BuildOutput<ErlangLanguage> {
        let parser = ErlangParser::new(self.config);
        let mut cache = data_text_core::parser::ParseSession::<ErlangLanguage>::default();
        let parse_result = parser.parse(source, edits, &mut cache);

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
