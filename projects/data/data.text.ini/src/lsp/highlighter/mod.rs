#![doc = include_str!("readme.md")]
use language_highlight::syntax::{HighlightSource, HighlightResult, TreeHighlighter};
use language_highlight::themes::Theme;
use data_text_core::{Language, errors::ParseResult};

/// Highlighter implementation for INI.
pub struct IniHighlighter {
    use_parser: bool,
}

impl IniHighlighter {
    /// Creates a new `IniHighlighter`.
    ///
    /// If `use_parser` is true, it will use the INI parser for highlighting.
    /// Otherwise, it will use the default language highlighter.
    pub fn new(use_parser: bool) -> Self {
        Self { use_parser }
    }
}

impl HighlightSource for IniHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, theme: Theme) -> ParseResult<HighlightResult<'a>> {
        let highlighter = TreeHighlighter::new();
        if self.use_parser {
            let config = crate::language::IniLanguage::default();
            let parser = crate::parser::IniParser::new(&config);
            let lexer = crate::lexer::IniLexer::new(&config);
            highlighter.highlight_with_language::<crate::language::IniLanguage, crate::parser::IniParser<'_>, crate::lexer::IniLexer>(source, theme, &parser, &lexer)
        }
        else {
            highlighter.highlight(source, crate::language::IniLanguage::NAME, theme)
        }
    }
}
