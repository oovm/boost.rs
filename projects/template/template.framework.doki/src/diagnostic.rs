use serde::{Deserialize, Serialize};

/// Structured diagnostic shared with N-API / CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostic {
    /// Stable machine code, e.g. `doki::config::invalid`.
    pub code: String,
    /// How severe the finding is.
    pub severity: Severity,
    /// Human-readable message.
    pub message: String,
    /// Project-relative file the finding belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Recovery hint for the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// Plugin that produced the finding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    /// Build or render phase the finding belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
}

/// Diagnostic severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Severity {
    /// Blocks the build.
    Error,
    /// Does not block, but needs attention.
    Warning,
    /// Purely informational.
    Info,
}

impl Diagnostic {
    /// Build an error-severity diagnostic with no location or hint.
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), severity: Severity::Error, message: message.into(), file: None, help: None, plugin: None, phase: None }
    }

    /// Build a warning-severity diagnostic with no location or hint.
    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), severity: Severity::Warning, message: message.into(), file: None, help: None, plugin: None, phase: None }
    }
}
