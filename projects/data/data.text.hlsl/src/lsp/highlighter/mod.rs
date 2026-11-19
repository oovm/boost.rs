#![doc = include_str!("readme.md")]
use data_text_core::{Language, errors::ParseResult};
use language_highlight::{
    syntax::{HighlightResult, HighlightSource, TreeHighlighter},
    themes::Theme,
};

pub struct HlslHighlighter {
    use_parser: bool,
}

impl HlslHighlighter {
    pub fn new(use_parser: bool) -> Self {
        Self { use_parser }
    }
}

impl HighlightSource for HlslHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, theme: Theme) -> ParseResult<HighlightResult<'a>> {
        let highlighter = TreeHighlighter::new();

        if self.use_parser {
            let config = crate::language::HlslLanguage::default();
            let parser = crate::parser::HlslParser::new(&config);
            let lexer = crate::lexer::HlslLexer::new(&config);
            highlighter.highlight_with_language::<crate::language::HlslLanguage, crate::parser::HlslParser<'_>, crate::lexer::HlslLexer<'_>>(
                source, theme, &parser, &lexer,
            )
        }
        else {
            highlighter.highlight(source, crate::language::HlslLanguage::NAME, theme)
        }
    }
}
