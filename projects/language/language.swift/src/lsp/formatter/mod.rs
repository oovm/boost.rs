#![doc = include_str!("readme.md")]
use data_text_swift::language::SwiftLanguage;
use data_text_core::RedNode;

pub struct SwiftFormatter<'config> {
    config: &'config SwiftLanguage,
}

impl<'config> SwiftFormatter<'config> {
    pub fn new(config: &'config SwiftLanguage) -> Self {
        Self { config }
    }

    pub fn format(&self, _root: &RedNode<SwiftLanguage>) -> String {
        String::new()
    }
}
