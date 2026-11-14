//! Event collection with nested spans.

use std::time::Instant;

use crate::event::{Event, EventKind, Seq, Severity};

/// One completed span.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Span {
    /// Caller-supplied name.
    pub name: String,
    /// Nesting depth, starting at zero.
    pub depth: usize,
    /// First sequence number recorded inside the span.
    pub start_seq: Seq,
    /// Last sequence number recorded inside the span.
    pub end_seq: Seq,
    /// Wall-clock duration in microseconds.
    pub elapsed_micros: u64,
}

/// A finished trace.
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Trace {
    /// Events in sequence order.
    pub events: Vec<Event>,
    /// Completed spans ordered by start sequence.
    pub spans: Vec<Span>,
}

impl Trace {
    /// Number of events at `severity`.
    pub fn count(&self, severity: Severity) -> usize {
        self.events.iter().filter(|event| event.severity == severity).count()
    }

    /// Whether nothing was recorded.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty() && self.spans.is_empty()
    }

    /// Total wall-clock time covered by top-level spans.
    pub fn elapsed_micros(&self) -> u64 {
        self.spans.iter().filter(|span| span.depth == 0).map(|span| span.elapsed_micros).sum()
    }
}

struct OpenSpan {
    name: String,
    start_seq: Seq,
    started: Instant,
    depth: usize,
}

/// Records events and spans.
#[derive(Default)]
pub struct Tracer {
    events: Vec<Event>,
    spans: Vec<Span>,
    open: Vec<OpenSpan>,
    next_seq: Seq,
}

impl Tracer {
    /// Creates an empty tracer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one event and returns its sequence number.
    pub fn record(&mut self, kind: EventKind) -> Seq {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.events.push(Event { seq, stage: kind.stage(), severity: kind.severity(), kind });
        seq
    }

    /// Runs `body` inside a named span and closes the span afterwards.
    pub fn span<T>(&mut self, name: impl Into<String>, body: impl FnOnce(&mut Self) -> T) -> T {
        let start_seq = self.next_seq;
        let depth = self.open.len();
        self.open.push(OpenSpan { name: name.into(), start_seq, started: Instant::now(), depth });
        let output = body(self);
        let open = self.open.pop().expect("span stack is balanced");
        let end_seq = self.next_seq.saturating_sub(1).max(open.start_seq);
        self.spans.push(Span {
            name: open.name,
            depth: open.depth,
            start_seq: open.start_seq,
            end_seq,
            elapsed_micros: open.started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64,
        });
        output
    }

    /// Finishes the trace, ordering spans by start sequence.
    pub fn finish(mut self) -> Trace {
        self.spans.sort_by_key(|span| span.start_seq);
        Trace { events: self.events, spans: self.spans }
    }
}
