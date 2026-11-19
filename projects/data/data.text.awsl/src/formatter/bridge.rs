//! AWSL adapter for the token-gap formatter.

use super::{options::FormatOptions, source::format_source};

/// Formats AWSL source through the CST token-gap path.
#[derive(Debug, Clone, Copy, Default)]
pub struct AwslCstFormatter;

impl AwslCstFormatter {
    /// Format one source buffer without constructing a pretty-print document.
    pub fn format_source(&self, source: &str, options: &FormatOptions) -> Result<String, data_text_core::TextError> {
        format_source(source, options)
    }
}
