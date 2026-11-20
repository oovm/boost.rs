//! Valkyrie formatter errors use the shared `data.text` diagnostic type.

/// Formatter diagnostics use the shared `data.text` error type.
pub type FormatError = data_text_core::errors::TextError;
