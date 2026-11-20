//! Formatter errors reuse the shared text diagnostic type.

pub use data_text_core::errors::{TextDiagnostics, TextError, TextErrorKind};

/// Result returned by the CST formatter algorithm.
pub type FormatResult<T> = Result<T, TextError>;
