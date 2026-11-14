//! Rule-based failure analysis over a recorded trace.

use crate::{
    event::{EventKind, Seq, Severity},
    tracer::Trace,
};

/// Stable identifier for one finding category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingCode {
    /// No backend was available or selected.
    NoBackend,
    /// Shader or program emission failed.
    EmitFailure,
    /// Dispatch failed.
    DispatchFailure,
    /// A read-back value disagreed with the reference value.
    ReadbackMismatch,
    /// A structural validation step rejected its input.
    ValidationFailure,
    /// A required capability is missing.
    CapabilityGap,
    /// Execution fell back to a less capable path.
    Degradation,
    /// A dispatch span exceeded the configured budget.
    SlowDispatch,
    /// Nothing was recorded.
    EmptyTrace,
}

impl FindingCode {
    /// Uppercase code used in reports.
    pub const fn label(self) -> &'static str {
        match self {
            Self::NoBackend => "NO_BACKEND",
            Self::EmitFailure => "EMIT_FAILURE",
            Self::DispatchFailure => "DISPATCH_FAILURE",
            Self::ReadbackMismatch => "READBACK_MISMATCH",
            Self::ValidationFailure => "VALIDATION_FAILURE",
            Self::CapabilityGap => "CAPABILITY_GAP",
            Self::Degradation => "DEGRADATION",
            Self::SlowDispatch => "SLOW_DISPATCH",
            Self::EmptyTrace => "EMPTY_TRACE",
        }
    }
}

/// One analysis result.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    /// Finding category.
    pub code: FindingCode,
    /// Severity assigned by the rule.
    pub severity: Severity,
    /// Human-readable summary.
    pub summary: String,
    /// Sequence numbers the finding is based on.
    pub evidence: Vec<Seq>,
    /// What to check next.
    pub suggestion: &'static str,
}

/// Tunables for [`analyze`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnalysisOptions {
    /// Dispatch spans slower than this are reported.
    pub slow_dispatch_micros: u64,
}

impl Default for AnalysisOptions {
    fn default() -> Self {
        Self { slow_dispatch_micros: 16_000 }
    }
}

/// Applies every rule to `trace`.
pub fn analyze(trace: &Trace, options: &AnalysisOptions) -> Vec<Finding> {
    let mut findings = Vec::new();
    if trace.is_empty() {
        findings.push(Finding {
            code: FindingCode::EmptyTrace,
            severity: Severity::Warning,
            summary: "no hardware events were recorded".into(),
            evidence: Vec::new(),
            suggestion: "wire the tracer into the backend probe, emit, dispatch, and readback paths",
        });
        return findings;
    }

    let mut probed = false;
    let mut available = false;
    let mut selected = false;
    for event in &trace.events {
        match &event.kind {
            EventKind::BackendProbed { available: ok, .. } => {
                probed = true;
                available |= *ok;
            }
            EventKind::BackendSelected { .. } => selected = true,
            EventKind::EmitFailed { isa, message } => findings.push(Finding {
                code: FindingCode::EmitFailure,
                severity: Severity::Error,
                summary: format!("`{isa}` emission failed: {message}"),
                evidence: vec![event.seq],
                suggestion: "check the program IR against the ISA encoder's supported subset",
            }),
            EventKind::DispatchFailed { task, message } => findings.push(Finding {
                code: FindingCode::DispatchFailure,
                severity: Severity::Error,
                summary: format!("dispatch `{task}` failed: {message}"),
                evidence: vec![event.seq],
                suggestion: "inspect resource bindings, dispatch bounds, and driver errors",
            }),
            EventKind::ReadbackMismatch { resource, index, expected, actual } => findings.push(Finding {
                code: FindingCode::ReadbackMismatch,
                severity: Severity::Error,
                summary: format!("resource {resource}[{index}] expected `{expected}` got `{actual}`"),
                evidence: vec![event.seq],
                suggestion: "compare the CPU reference interpreter with the device result for the same program",
            }),
            EventKind::ValidationFailed { subject, message } => findings.push(Finding {
                code: FindingCode::ValidationFailure,
                severity: Severity::Error,
                summary: format!("`{subject}` validation failed: {message}"),
                evidence: vec![event.seq],
                suggestion: "re-run the structural validator and inspect the reported ranges",
            }),
            EventKind::CapabilityGap { feature } => findings.push(Finding {
                code: FindingCode::CapabilityGap,
                severity: Severity::Warning,
                summary: format!("missing capability `{feature}`"),
                evidence: vec![event.seq],
                suggestion: "confirm the capability probe and the selected backend's feature set",
            }),
            EventKind::Degraded { from, to, reason } => findings.push(Finding {
                code: FindingCode::Degradation,
                severity: Severity::Warning,
                summary: format!("degraded `{from}` -> `{to}`: {reason}"),
                evidence: vec![event.seq],
                suggestion: "verify the preferred path is actually reachable on this machine",
            }),
            _ => {}
        }
    }

    if probed && !available && !selected {
        findings.push(Finding {
            code: FindingCode::NoBackend,
            severity: Severity::Error,
            summary: "every probed backend was unavailable".into(),
            evidence: trace
                .events
                .iter()
                .filter(|event| matches!(event.kind, EventKind::BackendProbed { .. }))
                .map(|event| event.seq)
                .collect(),
            suggestion: "install or enable a system-provided GPU API, or fall back to the CPU backend",
        });
    }

    for span in &trace.spans {
        if span.elapsed_micros > options.slow_dispatch_micros && is_dispatch_span(&span.name) {
            findings.push(Finding {
                code: FindingCode::SlowDispatch,
                severity: Severity::Warning,
                summary: format!("`{}` took {} us", span.name, span.elapsed_micros),
                evidence: vec![span.start_seq, span.end_seq],
                suggestion: "compare against the CPU reference timing and check for synchronization stalls",
            });
        }
    }

    findings
}

fn is_dispatch_span(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    lowered.contains("dispatch") || lowered.contains("render") || lowered.contains("draw")
}
