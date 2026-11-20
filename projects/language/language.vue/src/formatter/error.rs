//! Vue formatter diagnostics use the shared `data.text` error type.

/// Error returned by Vue source validation and formatting.
pub type FormatError = data_text_core::TextError;
