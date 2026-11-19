//! Vue formatter diagnostics use Oak's shared error type.

/// Error returned by Vue source validation and formatting.
pub type FormatError = data_text_core::OakError;
