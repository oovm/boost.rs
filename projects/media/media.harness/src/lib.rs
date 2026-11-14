//! Media execution tracing: what a render or synthesis run actually did.
//!
//! A host records resources, validation, synthesis, mixing, painting, encoding
//! and publishing as events and spans. This crate turns that record into
//! explainable findings and a Markdown report. It never opens a decoder, an
//! encoder or a GPU, and it owns no media algorithm — it only models what the
//! caller observed.
#![warn(missing_docs)]

mod analysis;
mod event;
mod level;
mod report;
mod tracer;

pub use analysis::{AnalysisOptions, Finding, FindingCode, analyze};
pub use event::{AssetKind, Event, EventKind, Seq, Severity, Stage};
pub use level::{LevelStats, SILENCE_FLOOR_DB, measure_f32, measure_pcm16};
pub use report::render_markdown;
pub use tracer::{Span, Trace, Tracer};
