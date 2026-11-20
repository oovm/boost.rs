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
            let config = data_text_hlsl::language::HlslLanguage::default();
            let parser = data_text_hlsl::parser::HlslParser::new(&config);
            let lexer = data_text_hlsl::lexer::HlslLexer::new(&config);
            highlighter.highlight_with_language::<data_text_hlsl::language::HlslLanguage, data_text_hlsl::parser::HlslParser<'_>, data_text_hlsl::lexer::HlslLexer<'_>>(
                source, theme, &parser, &lexer,
            )
        }
        else {
            highlighter.highlight(source, data_text_hlsl::language::HlslLanguage::NAME, theme)
        }
    }
}
