#![doc = include_str!("readme.md")]
use crate::{AdaParser, ast::*, language::AdaLanguage};
use data_text_core::{Builder, BuilderCache, GreenNode, TextDiagnostics, Parser, SourceText, TextEdit, source::Source};

/// AST builder for the Ada language.
#[derive(Clone)]
pub struct AdaBuilder<'config> {
    /// Language configuration.
    config: &'config AdaLanguage,
}

impl<'config> AdaBuilder<'config> {
    /// Creates a new Ada builder.
    pub fn new(config: &'config AdaLanguage) -> Self {
        Self { config }
    }

    /// Builds the AST root node from the syntax tree.
    pub fn build_root(&self, _green: &GreenNode<AdaLanguage>, _source: &SourceText) -> Result<AdaRoot, data_text_core::TextError> {
        // Simplified AST building logic
        Ok(AdaRoot::new(vec![]))
    }
}

impl<'config> Builder<AdaLanguage> for AdaBuilder<'config> {
    fn build<'a, S: Source + ?Sized>(&self, source: &'a S, edits: &[TextEdit], _cache: &'a mut impl BuilderCache<AdaLanguage>) -> data_text_core::builder::BuildOutput<AdaLanguage> {
        let parser = AdaParser::new(self.config);
        let mut cache = data_text_core::parser::ParseSession::<AdaLanguage>::default();
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
