#![doc = include_str!("readme.md")]
use crate::{ast::JavaScriptRoot, language::JavaScriptLanguage};
use data_text_core::{BuilderCache, TextDiagnostics, source::Source};
use data_text_pretty::PrinterConfig;

pub struct JavaScriptFormatter;

impl JavaScriptFormatter {
    pub fn format(
        &self,
        _root: &JavaScriptRoot,
        _text: impl Source,
        config: &PrinterConfig,
        _cache: impl BuilderCache<JavaScriptLanguage>,
    ) -> TextDiagnostics<String> {
        TextDiagnostics { result: Ok(String::new()), diagnostics: vec![] }
    }
}
