#![doc = include_str!("readme.md")]
use crate::{language::HaskellLanguage, lexer::HaskellLexer, parser::HaskellParser};
use data_text_core::{Language, errors::ParseResult};
use language_highlight::{
    syntax::{HighlightResult, HighlightSource, TreeHighlighter},
    themes::Theme,
};

/// Haskell syntax highlighter.
pub struct HaskellHighlighter {
    use_parser: bool,
}

impl HaskellHighlighter {
    pub fn new(use_parser: bool) -> Self {
        Self { use_parser }
    }
}

impl Default for HaskellHighlighter {
    fn default() -> Self {
        Self::new(true)
    }
}

impl HighlightSource for HaskellHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, theme: Theme) -> ParseResult<HighlightResult<'a>> {
        let highlighter = TreeHighlighter::new();

        if self.use_parser {
            let config = HaskellLanguage::default();
            let parser = HaskellParser::new(&config);
            let lexer = HaskellLexer::new(&config);
            highlighter.highlight_with_language::<HaskellLanguage, HaskellParser<'_>, HaskellLexer<'_>>(source, theme, &parser, &lexer)
        }
        else {
            highlighter.highlight(source, HaskellLanguage::NAME, theme)
        }
    }
}
