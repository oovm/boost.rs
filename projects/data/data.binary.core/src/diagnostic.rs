//! Binary domain diagnostic codes and builders on top of `boost-harness` diagnostics.
//!
//! Stable `binary.*` codes, budget usage, and conversion helpers. Rendering and
//! collection use the shared `boost_harness::diagnostic` module; this crate
//! never defines a second diagnostic core.

pub use boost_harness::diagnostic::{
    Diagnostic, DiagnosticAction, DiagnosticCode, DiagnosticLabel, DiagnosticLocation, DiagnosticOrigin, DiagnosticSet, DiagnosticSeverity,
    DiagnosticSink, Message, MessageArg,
};

/// Binary domain diagnostic code (maps to dotted `binary.*` wire identifiers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BinaryDiagnosticCode {
    /// Probe signature did not match any known format.
    MagicMismatch,
    /// Input ended before a declared structure completed.
    Truncated,
    /// Offset arithmetic overflowed the address space.
    OffsetOverflow,
    /// Offset points outside the available source range.
    OffsetOutOfRange,
    /// Field or member violates alignment constraints.
    Alignment,
    /// Reference graph contains a cycle.
    ReferenceCycle,
    /// Two structures claim the same byte range.
    Overlap,
    /// Checksum or hash validation failed.
    Checksum,
    /// Format version is recognized but unsupported.
    UnsupportedVersion,
    /// Parsing stopped because a budget limit was reached.
    ResourceLimit,
    /// Additional byte ranges are required to continue.
    NeedRange,
}

impl BinaryDiagnosticCode {
    /// Stable dotted wire identifier for this code.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MagicMismatch => "binary.probe.magic-mismatch",
            Self::Truncated => "binary.layout.truncated",
            Self::OffsetOverflow => "binary.layout.offset-overflow",
            Self::OffsetOutOfRange => "binary.layout.offset-out-of-range",
            Self::Alignment => "binary.layout.alignment",
            Self::ReferenceCycle => "binary.layout.reference-cycle",
            Self::Overlap => "binary.layout.overlap",
            Self::Checksum => "binary.layout.checksum",
            Self::UnsupportedVersion => "binary.layout.unsupported-version",
            Self::ResourceLimit => "binary.budget.resource-limit",
            Self::NeedRange => "binary.layout.need-range",
        }
    }

    /// Producer component for [`DiagnosticOrigin`].
    pub const fn component(self) -> &'static str {
        match self {
            Self::MagicMismatch => "probe",
            Self::ResourceLimit => "budget",
            _ => "layout",
        }
    }

    /// Convert to the shared wire diagnostic code type.
    pub fn wire_code(self) -> DiagnosticCode {
        DiagnosticCode::new(self.as_str())
    }
}

/// Build a binary error diagnostic with fallback text.
pub fn error(code: BinaryDiagnosticCode, message: impl Into<String>) -> Diagnostic {
    build(code, DiagnosticSeverity::Error, message)
}

/// Build a binary warning diagnostic with fallback text.
pub fn warning(code: BinaryDiagnosticCode, message: impl Into<String>) -> Diagnostic {
    build(code, DiagnosticSeverity::Warning, message)
}

/// Build a binary info diagnostic with fallback text.
pub fn info(code: BinaryDiagnosticCode, message: impl Into<String>) -> Diagnostic {
    build(code, DiagnosticSeverity::Info, message)
}

fn build(code: BinaryDiagnosticCode, severity: DiagnosticSeverity, message: impl Into<String>) -> Diagnostic {
    let fallback = message.into();
    Diagnostic::new(
        code.wire_code(),
        severity,
        DiagnosticOrigin::new("binary", code.component()),
        Message::new(code.as_str()).with_fallback(fallback),
    )
}

/// Structured recovery actions for binary diagnostics.
pub mod actions {
    use super::DiagnosticAction;

    /// Request additional byte ranges before continuing.
    pub fn provide_ranges() -> DiagnosticAction {
        DiagnosticAction::new("binary.provide-ranges")
    }

    /// Reduce parse phase or projection depth.
    pub fn reduce_scope() -> DiagnosticAction {
        DiagnosticAction::new("binary.reduce-scope")
    }

    /// Increase one or more parse budgets.
    pub fn increase_budget() -> DiagnosticAction {
        DiagnosticAction::new("binary.increase-budget")
    }

    /// Try a different adapter or format candidate.
    pub fn try_alternate_adapter() -> DiagnosticAction {
        DiagnosticAction::new("binary.try-alternate-adapter")
    }
}

/// Observable budget consumption attached to `ResourceLimited` results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BudgetUsage {
    /// Raw bytes read from sources.
    pub read_bytes: u64,
    /// Bytes materialized in decoded address spaces.
    pub decoded_bytes: u64,
    /// Layout nodes allocated.
    pub nodes: u64,
    /// Reference edges recorded or followed.
    pub references: u64,
    /// Pointer hops during traversal.
    pub pointer_hops: u64,
    /// Bytes scanned during signature or window search.
    pub scan_bytes: u64,
    /// Bytes retained in caches.
    pub cache_bytes: u64,
    /// Current envelope nesting depth.
    pub envelope_depth: u32,
    /// Container members indexed in the current query.
    pub container_members: u64,
}
