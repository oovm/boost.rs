//! ABC 前端冒烟：白名单子集可降到 `ScoreSpec` 并排版，其余以结构化诊断拒绝或记命名损失。

use std::ops::Range;

use plotter_engraving::spec::{Barline, Clef, EventKind, KeySignature, SlurState, TieState};
use plotter_input_abc::{abc_to_scene, abc_to_spec};
use plotter_types::codes;

fn span_of<'a>(source: &'a str, error: &plotter_engraving::EngravingError) -> &'a str {
    let span = error.span().unwrap_or_else(|| panic!("`{}` carries no span", error.message()));
    let Range { start, end } = span.clone();
    assert!(end <= source.len(), "span {start}..{end} leaves a {} byte source", source.len());
    &source[start..end]
}

const SMOKE: &str = "X:1\nT:Smoke\nM:4/4\nL:1/4\nK:C\nCDEF|\n";

#[test]
fn header_and_quarter_notes_lower_to_one_measure() {
    let lowered = abc_to_spec(SMOKE).expect("the subset should lower");
    let spec = &lowered.spec;

    assert_eq!(spec.title.as_deref(), Some("Smoke"));
    assert_eq!(spec.clef, Clef::Treble);
    assert_eq!(spec.key, KeySignature::Natural);
    assert_eq!(spec.measures.len(), 1);
    assert_eq!(spec.measures[0].events.len(), 4);
    assert!(spec.measures[0].events.iter().all(|event| event.duration == 1.0));
    assert_eq!(spec.measures[0].barline, Barline::Regular);
    assert!(lowered.diagnostics.is_empty(), "{:?}", lowered.diagnostics);
}

#[test]
fn key_signatures_follow_the_circle_of_fifths() {
    let g = abc_to_spec("X:1\nM:4/4\nL:1/4\nK:G\nGABc|\n").expect("G major lowers");
    assert_eq!(g.spec.key, KeySignature::Sharps(1));

    let f = abc_to_spec("X:1\nM:4/4\nL:1/4\nK:F\nFGA=B|\n").expect("F major lowers");
    assert_eq!(f.spec.key, KeySignature::Flats(1));

    let a_minor = abc_to_spec("X:1\nM:4/4\nL:1/4\nK:Am\nABCD|\n").expect("A minor lowers");
    assert_eq!(a_minor.spec.key, KeySignature::Natural, "A minor is relative to C major");
}

#[test]
fn accidentals_octaves_and_durations_are_preserved() {
    let lowered = abc_to_spec("X:1\nM:4/4\nL:1/8\nK:C\n^C, C2 c/2|\n").expect("parses");
    let events = &lowered.spec.measures[0].events;

    let EventKind::Note(first) = &events[0].kind
    else {
        panic!("first event is a note");
    };
    assert_eq!(first.octave, 3);
    assert_eq!(first.accidental, Some(plotter_engraving::spec::Accidental::Sharp));
    assert_eq!(events[0].duration, 0.5);
    assert_eq!(events[1].duration, 1.0, "`C2` doubles the eighth unit");
    assert_eq!(events[2].duration, 0.25);
}

#[test]
fn tie_slur_and_decorations_attach_to_events() {
    let lowered = abc_to_spec("X:1\nM:4/4\nL:1/4\nK:C\n(C-D) .E|\n").expect("parses");
    let events = &lowered.spec.measures[0].events;

    assert_eq!(events[0].tie, TieState::Start);
    assert_eq!(events[0].slur, SlurState::Start);
    assert_eq!(events[1].tie, TieState::Stop);
    assert_eq!(events[1].slur, SlurState::Stop);
    assert_eq!(events[2].articulations, vec![plotter_engraving::spec::Articulation::Staccato]);
}

#[test]
fn tuplets_compress_the_ratio() {
    let lowered = abc_to_spec("X:1\nM:4/4\nL:1/8\nK:C\n(3CDE F2|\n").expect("parses");
    let first = &lowered.spec.measures[0].events[0];
    let tuplet = first.tuplet.expect("a 3:2 tuplet");
    assert_eq!((tuplet.actual, tuplet.normal), (3, 2));
    assert!((first.ratio - 2.0 / 3.0).abs() < 1e-9);
    assert!((first.duration - 0.5).abs() < 1e-9, "duration keeps the written eighth, not the divided 1/3");
}

#[test]
fn a_six_eight_tune_with_triplets_engraves() {
    let source = "X:1\nM:6/8\nL:1/8\nK:A\n(3CDE F G|\n";
    let scene = abc_to_scene(source).expect("6/8 with a triplet lowers and engraves");
    assert!(scene.scene.size.width > 0.0);
}

#[test]
fn alto_clef_is_rejected_with_the_voice_span() {
    let source = "X:1\nM:4/4\nL:1/4\nK:C\nV:1 clef=alto\nCDEF|\n";
    let error = abc_to_spec(source).expect_err("alto clef is outside the subset");
    assert_eq!(error.code(), codes::ENGRAVING_UNSUPPORTED);
    assert!(span_of(source, &error).contains("clef=alto"));
}

#[test]
fn church_modes_are_rejected() {
    let error = abc_to_spec("X:1\nM:4/4\nL:1/4\nK:Ador\nABCD|\n").expect_err("dorian is outside the subset");
    assert_eq!(error.code(), codes::ENGRAVING_UNSUPPORTED);
    assert!(error.message().contains("dor"));
}

#[test]
fn double_barline_records_one_approximation() {
    let lowered = abc_to_spec("X:1\nM:4/4\nL:1/4\nK:C\nCDEF||\n").expect("parses");
    assert_eq!(lowered.spec.measures[0].barline, Barline::Regular);
    assert!(lowered.diagnostics.iter().any(|diagnostic| diagnostic.code().as_str() == codes::ENGRAVING_APPROXIMATED));
}

#[test]
fn a_score_lowers_to_a_non_empty_scene() {
    let scene = abc_to_scene(SMOKE).expect("smoke lowers and engraves");
    assert!(scene.scene.size.width > 0.0);
    assert!(scene.scene.size.height > 0.0);
    assert_eq!(scene.plan.systems.len(), 1);
}
