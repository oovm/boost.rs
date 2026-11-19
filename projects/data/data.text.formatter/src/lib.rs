//! CST token-gap formatting primitives for `data.text` language adapters.
//!
//! The code originates from the former `oak-formatter` project (MPL-2.0) and has
//! been internalized and de-branded for Boost.

#![warn(missing_docs)]

/// Formatting configuration and gap constraints.
pub mod config;
/// Shared formatting errors.
pub mod errors;
/// Source edit and token-gap operations.
pub mod formatters;

pub use crate::{
    config::{GapConstraint, GapFormatOptions},
    errors::{FormatResult, TextDiagnostics, TextError, TextErrorKind},
    formatters::{TextEdit, TokenGap, apply_edits, conservative_constraint, edits_for_gaps},
};
