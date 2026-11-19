//! Language-neutral diagnostics and the default stderr rendering.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::ports::Localizer;

/// Severity of one diagnostic row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Something failed.
    Error,
    /// Something is wrong but the run continues.
    Warning,
    /// Optional guidance.
    Advice,
}

/// One diagnostic row accepted at a command boundary.
///
/// The row carries a stable code plus typed safe args; natural language is
/// resolved by the host localizer, never baked into the row.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    /// Row severity.
    pub severity: Severity,
    /// Stable message id (`commander::*` or a domain code).
    pub code: String,
    /// Safe typed arguments for localization.
    pub args: BTreeMap<String, String>,
    /// Optional business message used only when no catalog template exists.
    pub message: Option<String>,
}

impl Diagnostic {
    /// Builds an error row.
    pub fn error(code: impl Into<String>, args: BTreeMap<String, String>) -> Self {
        Self { severity: Severity::Error, code: code.into(), args, message: None }
    }

    /// Builds a warning row.
    pub fn warning(code: impl Into<String>, args: BTreeMap<String, String>) -> Self {
        Self { severity: Severity::Warning, code: code.into(), args, message: None }
    }

    /// Builds an advice row.
    pub fn advice(code: impl Into<String>, args: BTreeMap<String, String>) -> Self {
        Self { severity: Severity::Advice, code: code.into(), args, message: None }
    }
}

/// Host-facing context handed to a registered diagnostic handler.
pub struct DiagContext<'a> {
    /// Resolved locale tag for this run.
    pub locale: &'a str,
    /// The run's localizer for on-demand interpolation.
    pub localizer: &'a dyn Localizer,
}

/// A registered diagnostic sink. When absent, the framework renders the row
/// to the stderr writer itself.
pub type DiagnosticHandler = Arc<dyn Fn(&Diagnostic, &DiagContext<'_>) + Send + Sync>;

/// Framework codes whose default rendering appends the "available options"
/// hint line, matching the TypeScript twin's `HINT_CODES`.
pub(crate) const HINT_CODES: &[&str] = &[
    "commander::unknown_command",
    "commander::unknown_option",
    "commander::missing_option_value",
    "commander::parse_failed",
    "commander::missing_required_option",
    "commander::option_not_in_enum",
    "commander::option_coerce_failed",
];

/// Localizes one key and treats the unresolved sentinel `{{key}}` as absent.
pub(crate) fn localized_or_id(
    localizer: &dyn Localizer,
    key: &str,
    args: &BTreeMap<String, String>,
) -> Option<String> {
    let value = localizer.t(key, args);
    if value == format!("{{{key}}}") { None } else { Some(value) }
}
