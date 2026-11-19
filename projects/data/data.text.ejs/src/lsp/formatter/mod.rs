#![doc = include_str!("readme.md")]
use crate::{ast::EjsRoot, language::EjsLanguage};
use data_text_core::{BuilderCache, TextDiagnostics, source::Source};
use oak_pretty_print::PrinterConfig;

/// Stub formatter for EJS templates.
pub struct EjsFormatter;

impl EjsFormatter {
    /// Formats an EJS root (stub).
    pub fn format(&self, _root: &EjsRoot, _text: impl Source, _config: &PrinterConfig, _cache: impl BuilderCache<EjsLanguage>) -> TextDiagnostics<String> {
        TextDiagnostics { result: Ok(String::new()), diagnostics: vec![] }
    }
}
