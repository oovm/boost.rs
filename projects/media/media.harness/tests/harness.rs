use media_harness::{
    AnalysisOptions, AssetKind, EventKind, FindingCode, LevelStats, Severity, Tracer, analyze, measure_f32, measure_pcm16, render_markdown,
};

fn trace_with(events: Vec<EventKind>) -> media_harness::Trace {
    let mut tracer = Tracer::new();
    for event in events {
        tracer.record(event);
    }
    tracer.finish()
}

#[test]
fn records_nested_spans_in_start_order() {
    let mut tracer = Tracer::new();
    tracer.span("render project", |tracer| {
        tracer.record(EventKind::ProjectLoaded { scenes: 1, frames: 60, fps: 30, width: 640, height: 360 });
        tracer.span("paint frames", |tracer| {
            tracer.record(EventKind::ScenePainted { scene: 0, frames: 60 });
        });
    });
    let trace = tracer.finish();

    assert_eq!(trace.events.len(), 2);
    assert_eq!(trace.spans.len(), 2);
    assert_eq!(trace.spans[0].name, "render project");
    assert_eq!(trace.spans[0].depth, 0);
    assert_eq!(trace.spans[1].name, "paint frames");
    assert_eq!(trace.spans[1].depth, 1);
    assert!(trace.spans[0].start_seq <= trace.spans[1].start_seq);
}

#[test]
fn required_and_optional_missing_assets_are_distinguished() {
    let trace = trace_with(vec![
        EventKind::AssetMissing { path: "a.png".into(), kind: AssetKind::Image, slot: "scene 0 item 1".into(), required: true },
        EventKind::AssetMissing { path: "b.wav".into(), kind: AssetKind::Audio, slot: "narration".into(), required: false },
    ]);
    let findings = analyze(&trace, &AnalysisOptions::default());
    assert!(findings.iter().any(|finding| finding.code == FindingCode::RequiredAssetMissing));
    assert!(findings.iter().any(|finding| finding.code == FindingCode::OptionalAssetSkipped));
    let required = findings.iter().find(|finding| finding.code == FindingCode::RequiredAssetMissing).unwrap();
    assert_eq!(required.severity, Severity::Error);
}

#[test]
fn synthesis_mix_paint_encode_and_publish_failures_are_findings() {
    let trace = trace_with(vec![
        EventKind::SynthesisFailed { subject: "impact".into(), message: "bad preset".into() },
        EventKind::MixFailed { message: "sample rate mismatch".into() },
        EventKind::PaintFailed { scene: 1, item: Some(2), message: "glyph missing".into() },
        EventKind::EncodeFailed { message: "encoder unavailable".into() },
        EventKind::PublishFailed { path: "out.mp4".into(), message: "locked".into() },
    ]);
    let findings = analyze(&trace, &AnalysisOptions::default());
    let codes: Vec<FindingCode> = findings.iter().map(|finding| finding.code).collect();
    for expected in [
        FindingCode::SynthesisFailure,
        FindingCode::MixFailure,
        FindingCode::PaintFailure,
        FindingCode::EncodeFailure,
        FindingCode::PublishFailure,
    ] {
        assert!(codes.contains(&expected), "missing {expected:?}");
    }
    assert!(findings.iter().all(|finding| finding.severity == Severity::Error));
}

#[test]
fn clipping_and_silence_are_reported_from_level_events() {
    let trace = trace_with(vec![
        EventKind::Synthesized { subject: "kick-808".into(), seconds: 1.0, peak_db: 0.0, rms_db: -8.0, clipped: true },
        EventKind::Mixed { seconds: 4.0, peak_db: -0.1, rms_db: -70.0, clipped: false },
    ]);
    let findings = analyze(&trace, &AnalysisOptions::default());
    let codes: Vec<FindingCode> = findings.iter().map(|finding| finding.code).collect();
    assert_eq!(codes.iter().filter(|code| **code == FindingCode::Clipping).count(), 1);
    assert!(codes.contains(&FindingCode::Silence));
}

#[test]
fn cancellation_frames_and_degradation_are_flagged() {
    let trace = trace_with(vec![
        EventKind::Cancelled { frame: 42 },
        EventKind::FrameMissed { scene: 0, frame: 43 },
        EventKind::Degraded { from: "gpu".into(), to: "cpu".into(), reason: "adapter lost".into() },
    ]);
    let findings = analyze(&trace, &AnalysisOptions::default());
    let codes: Vec<FindingCode> = findings.iter().map(|finding| finding.code).collect();
    assert!(codes.contains(&FindingCode::CancelledRun));
    assert!(codes.contains(&FindingCode::DroppedFrame));
    assert!(codes.contains(&FindingCode::Degradation));
}

#[test]
fn flags_empty_trace_and_slow_media_span() {
    let empty = analyze(&Tracer::new().finish(), &AnalysisOptions::default());
    assert_eq!(empty.len(), 1);
    assert_eq!(empty[0].code, FindingCode::EmptyTrace);

    let mut tracer = Tracer::new();
    tracer.span("paint frame 1", |tracer| {
        tracer.record(EventKind::ScenePainted { scene: 0, frames: 1 });
        std::thread::sleep(std::time::Duration::from_millis(2));
    });
    let trace = tracer.finish();
    let findings = analyze(&trace, &AnalysisOptions { slow_span_micros: 1, ..AnalysisOptions::default() });
    assert!(findings.iter().any(|finding| finding.code == FindingCode::SlowSpan));
}

#[test]
fn report_contains_audio_findings_and_timeline() {
    let trace = trace_with(vec![
        EventKind::ProjectLoaded { scenes: 1, frames: 30, fps: 30, width: 320, height: 180 },
        EventKind::Synthesized { subject: "whoosh".into(), seconds: 0.8, peak_db: -6.0, rms_db: -20.0, clipped: false },
        EventKind::Mixed { seconds: 1.0, peak_db: -3.0, rms_db: -18.0, clipped: false },
        EventKind::Encoded { frames: 30, bytes: 1024 },
        EventKind::Published { path: "out.mp4".into(), bytes: 1024 },
    ]);
    let findings = analyze(&trace, &AnalysisOptions::default());
    let report = render_markdown(&trace, &findings);

    assert!(report.contains("# Media execution trace report"));
    assert!(report.contains("## Audio"));
    assert!(report.contains("mixed peak: -3.0 dBFS"));
    assert!(report.contains("`whoosh` (0.80s)"));
    assert!(report.contains("## Timeline"));
    assert!(report.contains("published out.mp4"));
}

#[test]
fn level_measurement_reports_peak_rms_and_clipping() {
    let silence = measure_pcm16(&[0, 0, 0, 0]);
    assert!(silence.peak_db <= media_harness::SILENCE_FLOOR_DB + 0.01);
    assert_eq!(silence.clipped, 0);

    let loud = measure_pcm16(&[i16::MAX, i16::MIN, 0, 0]);
    assert_eq!(loud.clipped, 2);
    assert!(loud.peak_db >= -0.1, "peak {}", loud.peak_db);

    let float: LevelStats = measure_f32(&[0.5, -0.5, 1.0]);
    assert_eq!(float.clipped, 1);
    assert!((float.peak_db - 0.0).abs() < 0.01);
}
