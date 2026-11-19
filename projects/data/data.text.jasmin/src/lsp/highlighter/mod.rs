#![doc = include_str!("readme.md")]
use data_text_core::errors::ParseResult;
use language_highlight::syntax::{HighlightSource, HighlightResult, TreeHighlighter};
use language_highlight::themes::Theme;
use crate::language::JasminLanguage;

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
        let parser = crate::parser::JasminParser::new(&config);
        let lexer = crate::lexer::JasminLexer::new(&config);
        highlighter.highlight_with_language::<JasminLanguage, crate::parser::JasminParser<'_>, crate::lexer::JasminLexer<'_>>(source, theme, &parser, &lexer)
    }
}
