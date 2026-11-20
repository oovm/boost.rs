#![doc = include_str!("readme.md")]
use data_text_core::{Language, errors::ParseResult};
use language_highlight::{
    syntax::{HighlightResult, HighlightSource, TreeHighlighter},
    themes::Theme,
};

pub struct IdlHighlighter {
    use_parser: bool,
}

impl IdlHighlighter {
    pub fn new(use_parser: bool) -> Self {
        Self { use_parser }
    }
}

impl HighlightSource for IdlHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, theme: Theme) -> ParseResult<HighlightResult<'a>> {
        let highlighter = TreeHighlighter::new();

        if self.use_parser {
            let config = data_text_idl::language::IdlLanguage::default();
            let parser = data_text_idl::parser::IdlParser::new(&config);
            let lexer = data_text_idl::lexer::IdlLexer::new(&config);
            highlighter.highlight_with_language::<data_text_idl::language::IdlLanguage, data_text_idl::parser::IdlParser<'_>, data_text_idl::lexer::IdlLexer<'_>>(
                source, theme, &parser, &lexer,
            )
        }
        else {
            highlighter.highlight(source, data_text_idl::language::IdlLanguage::NAME, theme)
        }
    }
}
