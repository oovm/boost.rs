//! Structured log events, spans, filters, sinks, and global dispatch.
//!
//! The event model, fields, payloads, filtering, spans, tasks, and the sink trait build with
//! `no_std` + `alloc`. Global dispatch, file and JSON sinks, async sinks, stderr rendering, and
//! thread-local span tracking require the `std` feature.

#[macro_use]
mod macros;

#[cfg(feature = "std")]
mod async_sink;
mod event;
mod field;
#[cfg(feature = "std")]
mod file;
mod filter;
#[cfg(feature = "std")]
mod json;
mod level;
mod payload;
#[cfg(feature = "std")]
mod runtime;
mod sink;
mod span;
mod task;

#[cfg(feature = "std")]
pub mod console;

pub use crate::{field_value, span};
#[cfg(feature = "std")]
pub use crate::event;

#[cfg(feature = "std")]
pub use async_sink::{AsyncSink, AsyncSinkError};
#[cfg(feature = "std")]
pub use console::{StderrFallbackSink, StderrSubscriberSink, install_global_subscriber, render, render_plain};
pub use event::{EventKind, LogEvent, LogEventBuilder};
pub use field::{Field, FieldValue, Fields, IntoFieldValue};
#[cfg(feature = "std")]
pub use file::{FileSink, install_global_file_sink};
pub use filter::{Filter, FilterParseError};
#[cfg(feature = "std")]
pub use json::{JsonLinesSink, event_to_json};
pub use level::Level;
pub use payload::{
    ActionPayload, ByteRangePayload, CausePayload, DiagnosticPayload, LabelPayload, LocationPayload, MessagePayload, SourceRefPayload,
};
#[cfg(feature = "std")]
pub use runtime::{
    clear_global_sink, dropped_events, emit, flush_global_sink, reset_dropped_events, set_global_filter, set_global_sink, with_scope,
};
pub use sink::{FilteredSink, FlushOutcome, LogSink, VecSink};
pub use span::{LogSpan, SpanId};
#[cfg(feature = "std")]
pub use span::{SpanGuard, current_span_id};
pub use task::{TaskContext, TaskId, TaskRef};

/// Enter a span and return a guard that exits it on drop.
#[cfg(feature = "std")]
pub fn enter(span: LogSpan) -> SpanGuard {
    SpanGuard::enter(span)
}

/// Exit a span guard explicitly.
#[cfg(feature = "std")]
pub fn exit(guard: SpanGuard) {
    drop(guard);
}
