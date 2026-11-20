#![doc = include_str!("readme.md")]
use data_text_ejs::{ast::EjsRoot, language::EjsLanguage};
use data_text_core::{BuilderCache, TextDiagnostics, source::Source};
use data_text_pretty::PrinterConfig;

/// Stub formatter for EJS templates.
pub struct EjsFormatter;

impl EjsFormatter {
    /// Formats an EJS root (stub).
    pub fn format(
        &self,
        _root: &EjsRoot,
        _text: impl Source,
        _config: &PrinterConfig,
        _cache: impl BuilderCache<EjsLanguage>,
    ) -> TextDiagnostics<String> {
        TextDiagnostics { result: Ok(String::new()), diagnostics: vec![] }
    }
}
