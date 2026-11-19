#![doc = include_str!("readme.md")]
use crate::language::GsglLanguage;
use data_text_core::{Builder, BuilderCache, Source, TextEdit, builder::BuildOutput};

pub struct GsglBuilder {}

impl GsglBuilder {
    pub fn new(_lang: &GsglLanguage) -> Self {
        Self {}
    }
}

impl Builder<GsglLanguage> for GsglBuilder {
    fn build<'a, S: Source + ?Sized>(&self, _text: &S, _edits: &[TextEdit], _cache: &'a mut impl BuilderCache<GsglLanguage>) -> BuildOutput<GsglLanguage> {
        data_text_core::errors::TextDiagnostics { result: Err(data_text_core::errors::TextError::custom_error("Not implemented")), diagnostics: vec![] }
    }
}
