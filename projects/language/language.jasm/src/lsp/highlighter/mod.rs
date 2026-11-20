#![doc = include_str!("readme.md")]
use data_text_core::{Language, errors::ParseResult};
use language_highlight::{
    syntax::{HighlightResult, HighlightSource, TreeHighlighter},
    themes::Theme,
};

/// JASM syntax highlighter.
pub struct JasmHighlighter {
    use_parser: bool,
}

impl JasmHighlighter {
    /// Creates a new instance of `JasmHighlighter`.
    pub fn new(use_parser: bool) -> Self {
        Self { use_parser }
    }
}

impl HighlightSource for JasmHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, theme: Theme) -> ParseResult<HighlightResult<'a>> {
        let highlighter = TreeHighlighter::new();

        if self.use_parser {
            let config = data_text_jasm::language::JasmLanguage::default();
            let parser = data_text_jasm::parser::JasmParser::new(&config);
            let lexer = data_text_jasm::lexer::JasmLexer::new(&config);
            highlighter.highlight_with_language::<data_text_jasm::language::JasmLanguage, data_text_jasm::parser::JasmParser<'_>, data_text_jasm::lexer::JasmLexer>(
                source, theme, &parser, &lexer,
            )
        }
        else {
            highlighter.highlight(source, data_text_jasm::language::JasmLanguage::NAME, theme)
        }
    }
}
