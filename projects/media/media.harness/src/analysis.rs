//! Rule-based failure analysis over a recorded trace.
//!
//! Every rule is explainable: a finding names the sequence numbers it was
//! derived from and what to check next. No rule guesses at pixels or samples
//! the trace did not record.

use crate::{
    event::{EventKind, Seq, Severity},
    tracer::Trace,
};

/// Stable identifier for one finding category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum FindingCode {
    /// Nothing was recorded.
    EmptyTrace,
    /// A required asset was absent.
    RequiredAssetMissing,
    /// A non-required asset was absent and became documented silence.
    OptionalAssetSkipped,
    /// An asset existed but could not be decoded.
    AssetDecodeFailure,
    /// A font face could not be resolved.
    FontFailure,
    /// A project or scene contract check failed.
    ValidationFailure,
    /// Sound-effect or voice synthesis failed.
    SynthesisFailure,
    /// Mixing failed.
    MixFailure,
    /// Painting a frame or item failed.
    PaintFailure,
    /// Encoding failed.
    EncodeFailure,
    /// Publishing the artifact failed.
    PublishFailure,
    /// A requested frame was never painted.
    DroppedFrame,
    /// The run stopped on an explicit cancellation.
    CancelledRun,
    /// Execution fell back to a less capable path.
    Degradation,
    /// The mixed or synthesized audio reached full scale.
    Clipping,
    /// The mixed audio is effectively silent for a non-trivial duration.
    Silence,
    /// A paint, synthesis or encode span exceeded the configured budget.
    SlowSpan,
}

impl FindingCode {
    /// Uppercase code used in reports.
    pub const fn label(self) -> &'static str {
        match self {
            Self::EmptyTrace => "EMPTY_TRACE",
            Self::RequiredAssetMissing => "REQUIRED_ASSET_MISSING",
            Self::OptionalAssetSkipped => "OPTIONAL_ASSET_SKIPPED",
            Self::AssetDecodeFailure => "ASSET_DECODE_FAILURE",
            Self::FontFailure => "FONT_FAILURE",
            Self::ValidationFailure => "VALIDATION_FAILURE",
            Self::SynthesisFailure => "SYNTHESIS_FAILURE",
            Self::MixFailure => "MIX_FAILURE",
            Self::PaintFailure => "PAINT_FAILURE",
            Self::EncodeFailure => "ENCODE_FAILURE",
            Self::PublishFailure => "PUBLISH_FAILURE",
            Self::DroppedFrame => "DROPPED_FRAME",
            Self::CancelledRun => "CANCELLED_RUN",
            Self::Degradation => "DEGRADATION",
            Self::Clipping => "CLIPPING",
            Self::Silence => "SILENCE",
            Self::SlowSpan => "SLOW_SPAN",
        }
    }
}

/// One analysis result.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnalysisOptions {
    /// Paint, synthesis and encode spans slower than this are reported.
    pub slow_span_micros: u64,
    /// Mixed or synthesized peak at or above this dBFS counts as clipping.
    pub clip_ceiling_db: f32,
    /// Mixed RMS below this dBFS counts as silence.
    pub silence_floor_db: f32,
    /// Mixed beds shorter than this many seconds are never reported as silent.
    pub silence_min_seconds: f64,
}

impl Default for AnalysisOptions {
    fn default() -> Self {
        Self { slow_span_micros: 50_000, clip_ceiling_db: 0.0, silence_floor_db: -60.0, silence_min_seconds: 0.5 }
    }
}

/// Applies every rule to `trace`.
pub fn analyze(trace: &Trace, options: &AnalysisOptions) -> Vec<Finding> {
    let mut findings = Vec::new();
    if trace.is_empty() {
        findings.push(Finding {
            code: FindingCode::EmptyTrace,
            severity: Severity::Warning,
            summary: "no media execution events were recorded".into(),
            evidence: Vec::new(),
            suggestion: "wire the tracer into load, synthesize, mix, paint, encode and publish",
        });
        return findings;
    }

    for event in &trace.events {
        match &event.kind {
            EventKind::AssetMissing { path, kind, slot, required: true } => findings.push(Finding {
                code: FindingCode::RequiredAssetMissing,
                severity: Severity::Error,
                summary: format!("required {} `{path}` is missing at {slot}", kind.label()),
                evidence: vec![event.seq],
                suggestion: "restore the asset or declare the slot as explicitly optional",
            }),
            EventKind::AssetMissing { path, kind, slot, required: false } => findings.push(Finding {
                code: FindingCode::OptionalAssetSkipped,
                severity: Severity::Warning,
                summary: format!("optional {} `{path}` is missing at {slot}", kind.label()),
                evidence: vec![event.seq],
                suggestion: "confirm the silence or empty frame is intended, or supply the asset",
            }),
            EventKind::AssetDecodeFailed { path, kind, message } => findings.push(Finding {
                code: FindingCode::AssetDecodeFailure,
                severity: Severity::Error,
                summary: format!("{} `{path}` failed to decode: {message}", kind.label()),
                evidence: vec![event.seq],
                suggestion: "decode the asset independently and compare format, size and codec",
            }),
            EventKind::FontMissing { role, message } => findings.push(Finding {
                code: FindingCode::FontFailure,
                severity: Severity::Error,
                summary: format!("font `{role}` is unavailable: {message}"),
                evidence: vec![event.seq],
                suggestion: "declare explicit font files or install the system face the project expects",
            }),
            EventKind::ValidationFailed { subject, message } => findings.push(Finding {
                code: FindingCode::ValidationFailure,
                severity: Severity::Error,
                summary: format!("`{subject}` validation failed: {message}"),
                evidence: vec![event.seq],
                suggestion: "re-run the validator on the exact project JSON and inspect the reported field",
            }),
            EventKind::SynthesisFailed { subject, message } => findings.push(Finding {
                code: FindingCode::SynthesisFailure,
                severity: Severity::Error,
                summary: format!("`{subject}` synthesis failed: {message}"),
                evidence: vec![event.seq],
                suggestion: "resolve the spec or preset independently and check the parameter ranges",
            }),
            EventKind::MixFailed { message } => findings.push(Finding {
                code: FindingCode::MixFailure,
                severity: Severity::Error,
                summary: format!("mix failed: {message}"),
                evidence: vec![event.seq],
                suggestion: "check clip and narration sample rates against the project rate",
            }),
            EventKind::PaintFailed { scene, item, message } => findings.push(Finding {
                code: FindingCode::PaintFailure,
                severity: Severity::Error,
                summary: match item {
                    Some(item) => format!("paint scene {scene} item {item} failed: {message}"),
                    None => format!("paint scene {scene} failed: {message}"),
                },
                evidence: vec![event.seq],
                suggestion: "preview the failing frame and compare its assets, fonts and motion output",
            }),
            EventKind::FrameMissed { scene, frame } => findings.push(Finding {
                code: FindingCode::DroppedFrame,
                severity: Severity::Warning,
                summary: format!("scene {scene} never painted frame {frame}"),
                evidence: vec![event.seq],
                suggestion: "check the frame range, cancellation point and scene duration",
            }),
            EventKind::EncodeFailed { message } => findings.push(Finding {
                code: FindingCode::EncodeFailure,
                severity: Severity::Error,
                summary: format!("encode failed: {message}"),
                evidence: vec![event.seq],
                suggestion: "check encoder availability, output directory and the frame format",
            }),
            EventKind::PublishFailed { path, message } => findings.push(Finding {
                code: FindingCode::PublishFailure,
                severity: Severity::Error,
                summary: format!("publish `{path}` failed: {message}"),
                evidence: vec![event.seq],
                suggestion: "check that the target is not locked by another process and is writable",
            }),
            EventKind::Cancelled { frame } => findings.push(Finding {
                code: FindingCode::CancelledRun,
                severity: Severity::Warning,
                summary: format!("render cancelled at frame {frame}"),
                evidence: vec![event.seq],
                suggestion: "confirm the cancellation was requested; the previous output is kept",
            }),
            EventKind::Degraded { from, to, reason } => findings.push(Finding {
                code: FindingCode::Degradation,
                severity: Severity::Warning,
                summary: format!("degraded `{from}` -> `{to}`: {reason}"),
                evidence: vec![event.seq],
                suggestion: "verify the preferred path is actually reachable on this machine",
            }),
            EventKind::Synthesized { subject, peak_db, clipped, .. } => {
                if *clipped || *peak_db >= options.clip_ceiling_db {
                    findings.push(Finding {
                        code: FindingCode::Clipping,
                        severity: Severity::Warning,
                        summary: format!("`{subject}` reached {peak_db:.1} dBFS"),
                        evidence: vec![event.seq],
                        suggestion: "lower the placement volume or the preset gain before mixing",
                    });
                }
            }
            EventKind::Mixed { seconds, peak_db, rms_db, clipped } => {
                if *clipped || *peak_db >= options.clip_ceiling_db {
                    findings.push(Finding {
                        code: FindingCode::Clipping,
                        severity: Severity::Warning,
                        summary: format!("mixed bed reached {peak_db:.1} dBFS"),
                        evidence: vec![event.seq],
                        suggestion: "lower the loudest placement or add a gain envelope before encoding",
                    });
                }
                if *seconds >= options.silence_min_seconds && *rms_db < options.silence_floor_db {
                    findings.push(Finding {
                        code: FindingCode::Silence,
                        severity: Severity::Warning,
                        summary: format!("mixed bed is {rms_db:.1} dBFS RMS over {seconds:.2}s"),
                        evidence: vec![event.seq],
                        suggestion: "confirm the audio plan placed any clip or effect on the timeline",
                    });
                }
            }
            _ => {}
        }
    }

    for span in &trace.spans {
        if span.elapsed_micros > options.slow_span_micros && is_media_span(&span.name) {
            findings.push(Finding {
                code: FindingCode::SlowSpan,
                severity: Severity::Warning,
                summary: format!("`{}` took {} us", span.name, span.elapsed_micros),
                evidence: vec![span.start_seq, span.end_seq],
                suggestion: "compare the same span against a smaller project and check resource sizes",
            });
        }
    }

    findings
}

fn is_media_span(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    ["paint", "frame", "synthesi", "mix", "encode", "decode", "preflight", "render"].iter().any(|needle| lowered.contains(needle))
}
