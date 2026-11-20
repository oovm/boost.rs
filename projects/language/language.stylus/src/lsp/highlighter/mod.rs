#![doc = include_str!("readme.md")]
use data_text_stylus::language::StylusLanguage;
use data_text_core::{Language, errors::ParseResult};
use language_highlight::{
    syntax::{HighlightResult, HighlightSource, TreeHighlighter},
    themes::Theme,
};

pub struct StylusHighlighter {
    use_parser: bool,
}

impl StylusHighlighter {
    pub fn new(use_parser: bool) -> Self {
        Self { use_parser }
    }
}

impl HighlightSource for StylusHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, theme: Theme) -> ParseResult<HighlightResult<'a>> {
        let highlighter = TreeHighlighter::new();

        if self.use_parser {
            let config = StylusLanguage::new();
            let parser = data_text_stylus::parser::StylusParser::new(&config);
            let lexer = data_text_stylus::lexer::StylusLexer::new(&config);
            highlighter.highlight_with_language::<StylusLanguage, data_text_stylus::parser::StylusParser<'_>, data_text_stylus::lexer::StylusLexer>(
                source, theme, &parser, &lexer,
            )
        }
        else {
            highlighter.highlight(source, StylusLanguage::NAME, theme)
        }
    }
}
