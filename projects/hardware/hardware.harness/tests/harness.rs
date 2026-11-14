use hardware_harness::{AnalysisOptions, EventKind, FindingCode, Severity, Tracer, analyze, render_markdown};

fn trace_with(events: Vec<EventKind>) -> hardware_harness::Trace {
    let mut tracer = Tracer::new();
    for event in events {
        tracer.record(event);
    }
    tracer.finish()
}

#[test]
fn records_nested_spans_in_start_order() {
    let mut tracer = Tracer::new();
    tracer.span("probe", |tracer| {
        tracer.record(EventKind::BackendProbed { backend: "cpu".into(), available: true, reason: None });
        tracer.span("select", |tracer| {
            tracer.record(EventKind::BackendSelected { backend: "cpu".into(), reason: "default".into() });
        });
    });
    let trace = tracer.finish();

    assert_eq!(trace.events.len(), 2);
    assert_eq!(trace.spans.len(), 2);
    assert_eq!(trace.spans[0].name, "probe");
    assert_eq!(trace.spans[0].depth, 0);
    assert_eq!(trace.spans[1].name, "select");
    assert_eq!(trace.spans[1].depth, 1);
    assert!(trace.spans[0].start_seq <= trace.spans[1].start_seq);
}

#[test]
fn flags_emit_dispatch_and_readback_failures() {
    let trace = trace_with(vec![
        EventKind::EmitFailed { isa: "dxbc".into(), message: "unsupported op".into() },
        EventKind::DispatchFailed { task: "matmul".into(), message: "device lost".into() },
        EventKind::ReadbackMismatch { resource: 0, index: 3, expected: "1.0".into(), actual: "0.0".into() },
    ]);
    let findings = analyze(&trace, &AnalysisOptions::default());
    let codes: Vec<FindingCode> = findings.iter().map(|finding| finding.code).collect();
    assert!(codes.contains(&FindingCode::EmitFailure));
    assert!(codes.contains(&FindingCode::DispatchFailure));
    assert!(codes.contains(&FindingCode::ReadbackMismatch));
    assert!(findings.iter().all(|finding| finding.severity == Severity::Error));
}

#[test]
fn flags_missing_backend_when_every_probe_fails() {
    let trace = trace_with(vec![
        EventKind::BackendProbed { backend: "cuda".into(), available: false, reason: Some("no driver".into()) },
        EventKind::BackendProbed { backend: "cpu".into(), available: false, reason: Some("disabled".into()) },
    ]);
    let findings = analyze(&trace, &AnalysisOptions::default());
    assert!(findings.iter().any(|finding| finding.code == FindingCode::NoBackend));
}

#[test]
fn flags_empty_trace_and_slow_dispatch() {
    let empty = analyze(&Tracer::new().finish(), &AnalysisOptions::default());
    assert_eq!(empty.len(), 1);
    assert_eq!(empty[0].code, FindingCode::EmptyTrace);

    let mut tracer = Tracer::new();
    tracer.span("dispatch fill", |tracer| {
        tracer.record(EventKind::Dispatched { task: "fill".into(), parallel: 64 });
        std::thread::sleep(std::time::Duration::from_millis(2));
    });
    let trace = tracer.finish();
    let findings = analyze(&trace, &AnalysisOptions { slow_dispatch_micros: 1 });
    assert!(findings.iter().any(|finding| finding.code == FindingCode::SlowDispatch));
}

#[test]
fn report_contains_findings_and_timeline() {
    let trace = trace_with(vec![
        EventKind::BackendSelected { backend: "vulkan".into(), reason: "preferred".into() },
        EventKind::EmitFailed { isa: "spirv".into(), message: "bad module".into() },
    ]);
    let findings = analyze(&trace, &AnalysisOptions::default());
    let report = render_markdown(&trace, &findings);

    assert!(report.contains("# Hardware trace report"));
    assert!(report.contains("EMIT_FAILURE"));
    assert!(report.contains("BackendSelected") || report.contains("select `vulkan`"));
    assert!(report.contains("## Timeline"));
}
