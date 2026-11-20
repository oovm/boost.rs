#![doc = include_str!("readme.md")]
use data_text_jasmin::language::JasminLanguage;
use data_text_core::errors::ParseResult;
use language_highlight::{
    syntax::{HighlightResult, HighlightSource, TreeHighlighter},
    themes::Theme,
};

/// Jasmin highlighter implementation.
pub struct JasminHighlighter;

impl JasminHighlighter {
    /// Creates a new `JasminHighlighter`.
    pub fn new() -> Self {
        Self
    }
}

impl HighlightSource for JasminHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, theme: Theme) -> ParseResult<HighlightResult<'a>> {
        let highlighter = TreeHighlighter::new();
        let config = JasminLanguage::default();
        let parser = data_text_jasmin::parser::JasminParser::new(&config);
        let lexer = data_text_jasmin::lexer::JasminLexer::new(&config);
        highlighter.highlight_with_language::<JasminLanguage, data_text_jasmin::parser::JasminParser<'_>, data_text_jasmin::lexer::JasminLexer<'_>>(
            source, theme, &parser, &lexer,
        )
    }
}
