//! Hardware and render execution tracing.
//!
//! Callers record what the backend stack actually did, then ask this crate for
//! findings and a readable report. It never touches a driver and never runs
//! hardware work itself.
#![warn(missing_docs)]

mod analysis;
mod event;
mod report;
mod tracer;

pub use analysis::{AnalysisOptions, Finding, FindingCode, analyze};
pub use event::{Event, EventKind, Seq, Severity, Stage};
pub use report::render_markdown;
pub use tracer::{Span, Trace, Tracer};
