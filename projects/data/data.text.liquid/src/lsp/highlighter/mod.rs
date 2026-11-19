use language_highlight::syntax::{HighlightSource, HighlightResult, HighlightSegment, HighlightSpan};
use language_highlight::themes::Theme;
use crate::language::LiquidLanguage;
/// Highlighter module for Liquid
///
/// This module provides syntax highlighting support for Liquid templates.

/// Highlighter for Liquid templates
#[derive(Debug, Clone)]
pub struct LiquidHighlighter {
    theme: Theme,
}

impl LiquidHighlighter {
    /// Creates a new Liquid highlighter with the given theme
    pub fn new(theme: Theme) -> Self {
        Self { theme }
    }
}

impl HighlightSource for LiquidHighlighter {
    fn highlight<'a>(&self, source: &'a str, _language: &str, theme: Theme) -> Result<HighlightResult<'a>, data_text_core::errors::TextError> {
        let theme_config = theme.get_theme();
        let segments = vec![HighlightSegment { span: HighlightSpan { start: 0, end: source.len() }, style: theme_config.resolve_style("none"), text: std::borrow::Cow::Borrowed(source) }];
        Ok(HighlightResult { segments, source: std::borrow::Cow::Borrowed(source) })
    }
}
