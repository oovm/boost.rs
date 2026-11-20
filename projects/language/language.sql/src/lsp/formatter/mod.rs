#![doc = include_str!("readme.md")]
use data_text_sql::language::SqlLanguage;

/// A formatter for SQL source files.
pub struct SqlFormatter<'config> {
    config: &'config SqlLanguage,
}

impl<'config> SqlFormatter<'config> {
    /// Creates a new `SqlFormatter` with the given language configuration.
    pub fn new(config: &'config SqlLanguage) -> Self {
        Self { config }
    }

    /// Formats a SQL source tree into a string.
    pub fn format(&self, _node: &data_text_core::tree::RedNode<SqlLanguage>) -> String {
        // TODO: Implement SQL formatting
        String::new()
    }
}
