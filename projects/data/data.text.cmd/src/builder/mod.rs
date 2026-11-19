#![doc = include_str!("readme.md")]
use crate::{ast::*, language::CmdLanguage, parser::CmdParser};
use data_text_core::{Builder, BuilderCache, GreenNode, TextDiagnostics, Parser, SourceText, TextEdit, source::Source};

/// Windows Command (CMD) language AST builder
#[derive(Clone)]
pub struct CmdBuilder<'config> {
    config: &'config CmdLanguage,
}

impl<'config> CmdBuilder<'config> {
    /// Creates a new `CmdBuilder` with the given configuration.
    pub fn new(config: &'config CmdLanguage) -> Self {
        Self { config }
    }

    /// Builds the root AST node from the given green node.
    pub fn build_root(&self, _green: &GreenNode<CmdLanguage>, _source: &SourceText) -> Result<CmdRoot, data_text_core::TextError> {
        // Simplified AST building logic
        // This should build the tree according to actual CMD syntax rules
        // Currently just as an example
        Ok(CmdRoot { elements: vec![] })
    }
}

impl<'config> Builder<CmdLanguage> for CmdBuilder<'config> {
    fn build<'a, S: Source + ?Sized>(&self, source: &'a S, edits: &[TextEdit], _cache: &'a mut impl BuilderCache<CmdLanguage>) -> data_text_core::builder::BuildOutput<CmdLanguage> {
        let parser = CmdParser::new(self.config);
        let mut cache = data_text_core::parser::ParseSession::<CmdLanguage>::default();
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
