//! Markdown rendering for traces and findings.

use std::fmt::Write;

use crate::{
    analysis::Finding,
    event::{EventKind, Severity},
    tracer::Trace,
};

/// Renders a Markdown report with findings, audio facts, spans, and the event
/// timeline.
pub fn render_markdown(trace: &Trace, findings: &[Finding]) -> String {
    let mut out = String::new();
    let errors = trace.count(Severity::Error);
    let warnings = trace.count(Severity::Warning);
    let _ = writeln!(out, "# Media execution trace report");
    let _ = writeln!(out);
    let _ = writeln!(out, "- events: {} ({} errors, {} warnings)", trace.events.len(), errors, warnings);
    let _ = writeln!(out, "- spans: {}", trace.spans.len());
    let _ = writeln!(out, "- elapsed: {} us", trace.elapsed_micros());
    let _ = writeln!(out, "- findings: {}", findings.len());

    let _ = writeln!(out, "\n## Audio\n");
    match trace.last_level() {
        Some((_, peak_db, rms_db, clipped)) => {
            let _ = writeln!(out, "- mixed peak: {peak_db:.1} dBFS");
            let _ = writeln!(out, "- mixed rms: {rms_db:.1} dBFS");
            let _ = writeln!(out, "- clipped: {clipped}");
        }
        None => {
            let _ = writeln!(out, "No mixed audio was recorded.");
        }
    }
    let synthesized: Vec<String> = trace
        .events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Synthesized { subject, seconds, .. } => Some(format!("`{subject}` ({seconds:.2}s)")),
            _ => None,
        })
        .collect();
    if !synthesized.is_empty() {
        let _ = writeln!(out, "- synthesized: {}", synthesized.join(", "));
    }

    let _ = writeln!(out, "\n## Findings\n");
    if findings.is_empty() {
        let _ = writeln!(out, "No findings.");
    }
    for finding in findings {
        let _ = writeln!(out, "### [{}] {}", finding.severity.label(), finding.code.label());
        let _ = writeln!(out, "- {}", finding.summary);
        if !finding.evidence.is_empty() {
            let evidence: Vec<String> = finding.evidence.iter().map(|seq| format!("#{seq}")).collect();
            let _ = writeln!(out, "- evidence: {}", evidence.join(", "));
        }
        let _ = writeln!(out, "- next: {}", finding.suggestion);
        let _ = writeln!(out);
    }

    if !trace.spans.is_empty() {
        let _ = writeln!(out, "## Spans\n");
        let _ = writeln!(out, "| name | depth | elapsed (us) | events |");
        let _ = writeln!(out, "|------|-------|--------------|--------|");
        for span in &trace.spans {
            let _ = writeln!(out, "| {} | {} | {} | #{}..#{} |", span.name, span.depth, span.elapsed_micros, span.start_seq, span.end_seq);
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "## Timeline\n");
    let _ = writeln!(out, "| seq | stage | severity | event |");
    let _ = writeln!(out, "|-----|-------|----------|-------|");
    for event in &trace.events {
        let _ = writeln!(out, "| {} | {:?} | {} | {} |", event.seq, event.stage, event.severity.label(), event.kind.summary());
    }
    out
}
