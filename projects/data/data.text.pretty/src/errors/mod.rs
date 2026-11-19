use data_text_core::errors::TextError;

/// Formatting result type.
pub type FormatResult<T> = Result<T, TextError>;
