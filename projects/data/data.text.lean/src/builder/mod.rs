use crate::{ast::LeanRoot, language::LeanLanguage};
use data_text_core::{Builder, BuilderCache, GreenNode, TextDiagnostics, Parser, RedNode, TextEdit, source::Source};

/// Builder for constructing Lean AST from source text.
pub struct LeanBuilder<'config> {
    config: &'config LeanLanguage,
}

impl<'config> LeanBuilder<'config> {
    /// Creates a new Lean builder with the given language configuration.
    pub fn new(config: &'config LeanLanguage) -> Self {
        Self { config }
    }
}

impl<'config> Builder<LeanLanguage> for LeanBuilder<'config> {
    fn build<'a, S: Source + ?Sized>(&self, source: &'a S, edits: &[TextEdit], cache: &'a mut impl BuilderCache<LeanLanguage>) -> TextDiagnostics<LeanRoot> {
        let parser = crate::parser::LeanParser::new(self.config);
        let parse_result = parser.parse(source, edits, cache);

        match parse_result.result {
            Ok(green_tree) => {
                let ast_root = self.build_root(green_tree);
                TextDiagnostics { result: Ok(ast_root), diagnostics: parse_result.diagnostics }
            }
            Err(parse_error) => TextDiagnostics { result: Err(parse_error), diagnostics: parse_result.diagnostics },
        }
    }
}

impl<'config> LeanBuilder<'config> {
    pub(crate) fn build_root<'a>(&self, green_tree: &'a GreenNode<'a, LeanLanguage>) -> LeanRoot {
        LeanRoot::new(RedNode::new(green_tree, 0).span())
    }
}
