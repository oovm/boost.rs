#![doc = include_str!("readme.md")]
use language_highlight::syntax::{HighlightSource, HighlightResult};
use language_highlight::themes::Theme;
use crate::VerilogLanguage;

/// Highlighter implementation for Verilog.
pub struct VerilogHighlighter {}

impl HighlightSource for VerilogHighlighter {
    fn highlight<'a>(&self, source: &'a str, language: &str, theme: Theme) -> data_text_core::errors::ParseResult<HighlightResult<'a>> {
        let theme_config = theme.get_theme();
        Ok(HighlightResult { segments: vec![], source: std::borrow::Cow::Borrowed(source) })
    }
}
