#![deny(missing_docs)]
//! Shared check vocabulary for the Open* toolchain.
//!
//! A check pass produces [`Issue`] records. [`CheckReport`] collects them and
//! [`CheckReport::sorted`] returns a deterministic order, so a report file does
//! not churn when an internal hash map changes iteration order.
//!
//! [`CapabilityMatrix`] is the other half of the contract: a build states which
//! export targets, analyses, or operations it implements. Callers consult the
//! matrix before offering a feature instead of failing halfway through.

use std::{collections::BTreeMap, collections::BTreeSet, fmt};

use serde::{Deserialize, Serialize};

/// How much a finding matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Informational observation.
    Info,
    /// Something suspicious that does not block the result.
    Warning,
    /// Something that makes the result invalid.
    Error,
}

impl Severity {
    /// Stable lowercase keyword used in reports and JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    /// Whether this severity blocks acceptance.
    pub fn is_error(self) -> bool {
        self == Self::Error
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One finding with a stable machine code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// Stable diagnostic code, such as `drc/annular-ring`.
    pub code: String,
    /// How much the finding matters.
    pub severity: Severity,
    /// Human-readable message.
    pub message: String,
    /// Structured context, sorted by key on serialization.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub details: BTreeMap<String, String>,
}

impl Issue {
    /// Create an issue without details.
    pub fn new(code: impl Into<String>, severity: Severity, message: impl Into<String>) -> Self {
        Self { code: code.into(), severity, message: message.into(), details: BTreeMap::new() }
    }

    /// Create an informational issue.
    pub fn info(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, Severity::Info, message)
    }

    /// Create a warning.
    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, Severity::Warning, message)
    }

    /// Create a blocking error.
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, Severity::Error, message)
    }

    /// Attach one structured detail.
    pub fn with_detail(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.details.insert(key.into(), value.into());
        self
    }
}

/// A deterministic report of issues plus counts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckReport {
    /// Findings in insertion order. Use [`Self::sorted`] for report output.
    pub issues: Vec<Issue>,
}

impl CheckReport {
    /// Create an empty report.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of findings.
    pub fn len(&self) -> usize {
        self.issues.len()
    }

    /// Whether no finding was recorded.
    pub fn is_empty(&self) -> bool {
        self.issues.is_empty()
    }

    /// Append one finding.
    pub fn push(&mut self, issue: Issue) {
        self.issues.push(issue);
    }

    /// Append every finding of another report.
    pub fn extend(&mut self, other: CheckReport) {
        self.issues.extend(other.issues);
    }

    /// Findings with exactly this severity.
    pub fn count(&self, severity: Severity) -> usize {
        self.issues.iter().filter(|issue| issue.severity == severity).count()
    }

    /// Whether any finding is an error.
    pub fn has_errors(&self) -> bool {
        self.issues.iter().any(|issue| issue.severity.is_error())
    }

    /// Whether the report carries no error, which is the acceptance test.
    pub fn is_ok(&self) -> bool {
        !self.has_errors()
    }

    /// Findings ordered errors first, then by code and message.
    ///
    /// The order does not depend on insertion order, so two passes that find
    /// the same issues produce the same report text.
    pub fn sorted(&self) -> Vec<&Issue> {
        let mut issues: Vec<&Issue> = self.issues.iter().collect();
        issues.sort_by(|left, right| {
            right.severity.cmp(&left.severity).then_with(|| left.code.cmp(&right.code)).then_with(|| left.message.cmp(&right.message))
        });
        issues
    }

    /// Sort the stored findings in place, using the order of [`Self::sorted`].
    pub fn normalize(&mut self) {
        self.issues.sort_by(|left, right| {
            right.severity.cmp(&left.severity).then_with(|| left.code.cmp(&right.code)).then_with(|| left.message.cmp(&right.message))
        });
    }
}

/// How completely a capability is implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Support {
    /// Fully implemented and covered by tests.
    Supported,
    /// Implemented with a documented limitation.
    Partial,
    /// Declared but not implemented yet.
    Planned,
    /// Deliberately not offered.
    Unsupported,
}

impl Support {
    /// Stable lowercase keyword used in capability tables.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Partial => "partial",
            Self::Planned => "planned",
            Self::Unsupported => "unsupported",
        }
    }

    /// Whether a caller may invoke the capability.
    pub fn is_available(self) -> bool {
        matches!(self, Self::Supported | Self::Partial)
    }
}

impl fmt::Display for Support {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One row of a capability matrix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    /// Stable capability identity, such as `export.png`.
    pub id: String,
    /// Family the capability belongs to, such as `export`.
    pub family: String,
    /// Implementation level.
    pub support: Support,
    /// Optional limitation note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Capability {
    /// Create a capability row without a note.
    pub fn new(id: impl Into<String>, family: impl Into<String>, support: Support) -> Self {
        Self { id: id.into(), family: family.into(), support, note: None }
    }

    /// Attach a limitation note.
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}

/// The set of capabilities one build advertises.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityMatrix {
    /// Rows in declaration order. Use [`Self::sorted`] for output.
    pub entries: Vec<Capability>,
}

impl CapabilityMatrix {
    /// Create an empty matrix.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one capability row.
    pub fn insert(&mut self, capability: Capability) {
        self.entries.push(capability);
    }

    /// Find a row by identity.
    pub fn get(&self, id: &str) -> Option<&Capability> {
        self.entries.iter().find(|capability| capability.id == id)
    }

    /// Whether the capability is available.
    ///
    /// An unknown capability is unavailable, so a caller cannot silently use
    /// something this build never declared.
    pub fn supports(&self, id: &str) -> bool {
        self.get(id).is_some_and(|capability| capability.support.is_available())
    }

    /// Distinct families present, in ascending order.
    pub fn families(&self) -> BTreeSet<&str> {
        self.entries.iter().map(|capability| capability.family.as_str()).collect()
    }

    /// Rows ordered by family and then identity.
    pub fn sorted(&self) -> Vec<&Capability> {
        let mut entries: Vec<&Capability> = self.entries.iter().collect();
        entries.sort_by(|left, right| left.family.cmp(&right.family).then_with(|| left.id.cmp(&right.id)));
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_orders_info_before_error() {
        assert!(Severity::Info < Severity::Warning);
        assert!(Severity::Warning < Severity::Error);
        assert_eq!(Severity::Error.to_string(), "error");
    }

    #[test]
    fn report_sorts_errors_first_then_code() {
        let mut report = CheckReport::new();
        report.push(Issue::warning("drc/width", "narrow"));
        report.push(Issue::error("route/unrouted", "no path"));
        report.push(Issue::error("drc/ring", "thin ring"));
        let codes: Vec<&str> = report.sorted().iter().map(|issue| issue.code.as_str()).collect();
        assert_eq!(codes, ["drc/ring", "route/unrouted", "drc/width"]);
        assert!(report.has_errors());
        assert!(!report.is_ok());
    }

    #[test]
    fn report_details_serialize_in_key_order() {
        let issue = Issue::error("drc/ring", "thin").with_detail("net", "GND").with_detail("layer", "F.Cu");
        let json = serde_json::to_string(&issue).expect("serialize");
        let layer = json.find("layer").expect("layer key");
        let net = json.find("net").expect("net key");
        assert!(layer < net);
    }

    #[test]
    fn matrix_reports_availability() {
        let mut matrix = CapabilityMatrix::new();
        matrix.insert(Capability::new("export.svg", "export", Support::Supported));
        matrix.insert(Capability::new("export.png", "export", Support::Planned));
        assert!(matrix.supports("export.svg"));
        assert!(!matrix.supports("export.png"));
        assert!(!matrix.supports("export.unknown"));
        let ids: Vec<&str> = matrix.sorted().iter().map(|capability| capability.id.as_str()).collect();
        assert_eq!(ids, ["export.png", "export.svg"]);
    }

    #[test]
    fn report_round_trips_through_json() {
        let mut report = CheckReport::new();
        report.push(Issue::warning("check/generic", "look").with_detail("count", "2"));
        let json = serde_json::to_string(&report).expect("serialize");
        let restored: CheckReport = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored, report);
    }
}
