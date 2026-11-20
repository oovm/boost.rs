#![doc = include_str!("readme.md")]

use language_highlight::{
    syntax::{HighlightResult, HighlightSource},
    themes::Theme,
};
/// Highlighter implementation for Less.
pub struct LessHighlighter {}

impl HighlightSource for LessHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, theme: Theme) -> data_text_core::errors::ParseResult<HighlightResult<'a>> {
        let _theme_config = theme.get_theme();
        Ok(HighlightResult { segments: vec![], source: std::borrow::Cow::Borrowed(source) })
    }
}
