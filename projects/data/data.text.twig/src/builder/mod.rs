use crate::{ast::TwigRoot, language::TwigLanguage, lexer::TwigLexer, parser::TwigParser};
use data_text_core::{Builder, BuilderCache, GreenNode, RedNode, SourceText, TextDiagnostics, TextEdit, TextError, source::Source};

/// AST builder for Twig language
#[derive(Clone)]
pub struct TwigBuilder<'config> {
    /// Language configuration
    config: &'config TwigLanguage,
}

impl<'config> TwigBuilder<'config> {
    /// Creates a new Twig builder
    pub fn new(config: &'config TwigLanguage) -> Self {
        Self { config }
    }
}

impl<'config> Builder<TwigLanguage> for TwigBuilder<'config> {
    fn build<'a, S: Source + ?Sized>(
        &self,
        source: &S,
        edits: &[TextEdit],
        _cache: &'a mut impl BuilderCache<TwigLanguage>,
    ) -> data_text_core::builder::BuildOutput<TwigLanguage> {
        let parser = TwigParser::new(self.config);
        let lexer = TwigLexer::new(&self.config);

        let mut cache = data_text_core::parser::session::ParseSession::<TwigLanguage>::default();
        let parse_result = data_text_core::parser::parse(&parser, &lexer, source, edits, &mut cache);

        match parse_result.result {
            Ok(green_tree) => {
                let source_text = SourceText::new(source.get_text_in((0..source.length()).into()).into_owned());
                match self.build_root(green_tree.clone(), &source_text) {
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

impl<'config> TwigBuilder<'config> {
    /// Builds the root node
    pub(crate) fn build_root(&self, green_tree: GreenNode<TwigLanguage>, _source: &SourceText) -> Result<TwigRoot, TextError> {
        let red_root = RedNode::new(&green_tree, 0);
        let span = red_root.span();

        Ok(TwigRoot { span: span.into() })
    }
}
