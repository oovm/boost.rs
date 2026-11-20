#![doc = include_str!("readme.md")]
use data_text_stylus::language::StylusLanguage;
use data_text_core::{RedNode, TextEdit};

pub struct StylusFormatter;

impl StylusFormatter {
    pub fn new(config: &StylusLanguage) -> Self {
        Self
    }
}

impl StylusFormatter {
    pub fn format(&self, _root: &RedNode<StylusLanguage>) -> Vec<TextEdit> {
        vec![]
    }
}
