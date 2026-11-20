#![doc = include_str!("readme.md")]
use data_text_core::{Language, errors::ParseResult};
use language_highlight::{
    syntax::{HighlightResult, HighlightSource, TreeHighlighter},
    themes::Theme,
};

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
            let config = data_text_ini::language::IniLanguage::default();
            let parser = data_text_ini::parser::IniParser::new(&config);
            let lexer = data_text_ini::lexer::IniLexer::new(&config);
            highlighter.highlight_with_language::<data_text_ini::language::IniLanguage, data_text_ini::parser::IniParser<'_>, data_text_ini::lexer::IniLexer>(
                source, theme, &parser, &lexer,
            )
        }
        else {
            highlighter.highlight(source, data_text_ini::language::IniLanguage::NAME, theme)
        }
    }
}
