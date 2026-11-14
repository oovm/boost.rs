//! Recorded events and their classification.

/// Monotonic event sequence number inside one [`Trace`](crate::Trace).
pub type Seq = u64;

/// How serious an event is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Severity {
    /// Verbose diagnostic detail.
    Trace,
    /// Normal progress.
    Info,
    /// Something degraded but kept running.
    Warning,
    /// An operation failed.
    Error,
}

impl Severity {
    /// Uppercase label used in reports.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Info => "INFO",
            Self::Warning => "WARN",
            Self::Error => "ERROR",
        }
    }
}

/// Execution phase an event belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Stage {
    /// Backend and capability probing.
    Probe,
    /// Backend or strategy selection.
    Select,
    /// Resource preparation.
    Prepare,
    /// Shader or program emission.
    Emit,
    /// Kernel or draw dispatch.
    Dispatch,
    /// Result readback.
    Readback,
    /// Teardown.
    Teardown,
    /// Anything that does not fit the other stages.
    Other,
}

/// One recorded fact.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum EventKind {
    /// A backend was probed for availability.
    BackendProbed {
        /// Backend name.
        backend: String,
        /// Whether the probe succeeded.
        available: bool,
        /// Why the probe failed, when it did.
        reason: Option<String>,
    },
    /// A backend was chosen for execution.
    BackendSelected {
        /// Backend name.
        backend: String,
        /// Why this backend was chosen.
        reason: String,
    },
    /// A backend reported an internal probe, such as a module cache lookup.
    BackendProbe {
        /// Backend name.
        backend: String,
        /// Stable probe category (`module_compile`, `module_cache_hit`, …).
        kind: String,
        /// Content hash key of the shader module, when known.
        shader_key: Option<u64>,
    },
    /// A required capability is missing on the chosen backend.
    CapabilityGap {
        /// Missing feature name.
        feature: String,
    },
    /// Execution fell back to a less capable path.
    Degraded {
        /// Path that was requested.
        from: String,
        /// Path that actually ran.
        to: String,
        /// Why the fallback happened.
        reason: String,
    },
    /// A shader or program was emitted.
    Emitted {
        /// Target ISA name.
        isa: String,
        /// Entry point name.
        entry: String,
        /// Byte length of the module.
        bytes: usize,
    },
    /// Emission failed.
    EmitFailed {
        /// Target ISA name.
        isa: String,
        /// Failure message.
        message: String,
    },
    /// A task was dispatched.
    Dispatched {
        /// Task name.
        task: String,
        /// Parallel instance count.
        parallel: u32,
    },
    /// Dispatch failed.
    DispatchFailed {
        /// Task name.
        task: String,
        /// Failure message.
        message: String,
    },
    /// A resource was read back.
    Readback {
        /// Resource slot.
        resource: u32,
        /// Byte length read.
        bytes: usize,
    },
    /// A read-back value disagreed with the reference value.
    ReadbackMismatch {
        /// Resource slot.
        resource: u32,
        /// Element index.
        index: usize,
        /// Expected value.
        expected: String,
        /// Actual value.
        actual: String,
    },
    /// A structural validation step rejected its input.
    ValidationFailed {
        /// What was validated.
        subject: String,
        /// Failure message.
        message: String,
    },
    /// Free-form diagnostic note.
    Note {
        /// Note text.
        message: String,
    },
}

impl EventKind {
    /// Severity implied by the event kind.
    pub const fn severity(&self) -> Severity {
        match self {
            Self::EmitFailed { .. } | Self::DispatchFailed { .. } | Self::ReadbackMismatch { .. } | Self::ValidationFailed { .. } => {
                Severity::Error
            }
            Self::CapabilityGap { .. } | Self::Degraded { .. } => Severity::Warning,
            Self::BackendProbed { available: true, .. } => Severity::Info,
            Self::BackendProbed { .. } => Severity::Warning,
            Self::BackendSelected { .. } | Self::Emitted { .. } | Self::Dispatched { .. } | Self::Readback { .. } => Severity::Info,
            Self::Note { .. } | Self::BackendProbe { .. } => Severity::Trace,
        }
    }

    /// Execution phase implied by the event kind.
    pub const fn stage(&self) -> Stage {
        match self {
            Self::BackendProbed { .. } | Self::BackendProbe { .. } => Stage::Probe,
            Self::BackendSelected { .. } => Stage::Select,
            Self::CapabilityGap { .. } | Self::Degraded { .. } => Stage::Select,
            Self::Emitted { .. } | Self::EmitFailed { .. } => Stage::Emit,
            Self::Dispatched { .. } | Self::DispatchFailed { .. } => Stage::Dispatch,
            Self::Readback { .. } | Self::ReadbackMismatch { .. } => Stage::Readback,
            Self::ValidationFailed { .. } | Self::Note { .. } => Stage::Other,
        }
    }

    /// Short human-readable summary for timelines.
    pub fn summary(&self) -> String {
        match self {
            Self::BackendProbed { backend, available, reason } => match reason {
                Some(reason) => format!("probe `{backend}` available={available}: {reason}"),
                None => format!("probe `{backend}` available={available}"),
            },
            Self::BackendProbe { backend, kind, shader_key } => match shader_key {
                Some(key) => format!("probe `{backend}` {kind} key={key:#x}"),
                None => format!("probe `{backend}` {kind}"),
            },
            Self::BackendSelected { backend, reason } => format!("select `{backend}`: {reason}"),
            Self::CapabilityGap { feature } => format!("capability gap `{feature}`"),
            Self::Degraded { from, to, reason } => format!("degraded `{from}` -> `{to}`: {reason}"),
            Self::Emitted { isa, entry, bytes } => format!("emit `{entry}` as `{isa}` ({bytes} bytes)"),
            Self::EmitFailed { isa, message } => format!("emit `{isa}` failed: {message}"),
            Self::Dispatched { task, parallel } => format!("dispatch `{task}` x{parallel}"),
            Self::DispatchFailed { task, message } => format!("dispatch `{task}` failed: {message}"),
            Self::Readback { resource, bytes } => format!("readback resource {resource} ({bytes} bytes)"),
            Self::ReadbackMismatch { resource, index, expected, actual } => {
                format!("readback resource {resource}[{index}] expected `{expected}` got `{actual}`")
            }
            Self::ValidationFailed { subject, message } => format!("validation `{subject}` failed: {message}"),
            Self::Note { message } => message.clone(),
        }
    }
}

/// One recorded event with its sequence number.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Event {
    /// Monotonic sequence number.
    pub seq: Seq,
    /// Execution phase.
    pub stage: Stage,
    /// Event severity.
    pub severity: Severity,
    /// Recorded fact.
    pub kind: EventKind,
}
