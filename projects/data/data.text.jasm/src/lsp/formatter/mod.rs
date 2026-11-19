//! Formatter implementation for the JASM language.

use data_text_core::source::{SourceBuffer, ToSource};
use data_text_pretty::to_doc::AsDocument;
use data_text_pretty::{Document, PrinterConfig};

use crate::ast::JasmRoot;

/// Formatter for the JASM language.
pub struct JasmFormatter {
    /// Indentation size.
    indent_size: usize,
}

impl JasmFormatter {
    /// Creates a new `JasmFormatter` with default settings.
    pub fn new() -> Self {
        Self { indent_size: 4 }
    }

    /// Formats a JASM root node into a string.
    pub fn format(&self, root: &JasmRoot) -> String {
        let mut buffer = SourceBuffer::new();
        root.to_source(&mut buffer);
        buffer.to_string()
    }

    /// Formats a JASM root node using pretty-printing.
    pub fn format_pretty(&self, root: &JasmRoot) -> String {
        let doc = root.as_document(&Default::default());
        doc.render()
    }
}

impl Default for JasmFormatter {
    fn default() -> Self {
        Self::new()
    }
}
