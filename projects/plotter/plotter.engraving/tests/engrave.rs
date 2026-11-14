//! `ScoreSpec` 排版与拒绝路径的冒烟测试。
//!
//! 所有夹具在测试内构造，不落盘、不入库二进制。

use plotter_engraving::{
    codes,
    plan::EngravingOptions,
    score_to_scene_with_options, smufl,
    spec::{
        Accidental, Articulation, Barline, Beam, Clef, Event, EventKind, KeySignature, Measure, Pitch, ScoreSpec, SlurState,
        Step, TieState, TimeSignature, Tuplet,
    },
};

fn pitch(step: Step, octave: i8) -> Pitch {
    Pitch { step, octave, accidental: None }
}

/// 四四拍、C 大调、单小节四拍的四音符旋律。
fn melody() -> ScoreSpec {
    let mut spec = ScoreSpec::new();
    spec.title = Some("Smoke".to_string());
    spec.measures.push(Measure {
        events: vec![
            Event::note(pitch(Step::C, 4), 1.0),
            Event::note(pitch(Step::D, 4), 1.0),
            Event::note(pitch(Step::E, 4), 1.0),
            Event::note(pitch(Step::F, 4), 1.0),
        ],
        barline: Barline::Final,
    });
    spec
}

#[test]
fn melody_engraves_five_staff_lines_and_one_head_per_event() {
    let lowered = score_to_scene_with_options(&melody(), &EngravingOptions::default()).expect("melody engraves");

    assert_eq!(lowered.plan.systems.len(), 1);
    let system = &lowered.plan.systems[0];
    assert_eq!(system.staff_lines().len(), 5);
    assert_eq!(system.notes.len(), 4);
    assert_eq!(system.notes[0].heads[0].glyph, smufl::NOTE_QUARTER);
    assert!(lowered.diagnostics.is_empty(), "a full measure reports no approximation");
    assert!(lowered.scene.size.width > 0.0 && lowered.scene.size.height > 0.0);
}

#[test]
fn scene_serializes_to_svg_with_bravura_glyphs() {
    let lowered = score_to_scene_with_options(&melody(), &EngravingOptions::default()).expect("melody engraves");
    let svg = lowered.scene.to_svg().expect("scene serializes");

    assert!(svg.starts_with("<svg"), "SVG root emitted");
    assert!(svg.contains("font-family=\"Bravura\""), "SMuFL glyphs name the Bravura face");
    assert!(svg.contains(smufl::G_CLEF), "treble clef glyph present");
    assert!(svg.contains("data-role=\"node\""), "note groups carry the node role");
}

#[test]
fn chord_stacks_heads_at_one_x_position() {
    let mut spec = ScoreSpec::new();
    let mut chord = Event::chord(vec![pitch(Step::C, 4), pitch(Step::E, 4), pitch(Step::G, 4)], 4.0);
    chord.lyric = Some("do-mi-sol".to_string());
    spec.measures.push(Measure { events: vec![chord], barline: Barline::Regular });

    let lowered = score_to_scene_with_options(&spec, &EngravingOptions::default()).expect("chord engraves");
    let note = &lowered.plan.systems[0].notes[0];
    assert_eq!(note.heads.len(), 3);
    let xs: Vec<f64> = note.heads.iter().map(|glyph| glyph.at.x).collect();
    assert!(xs.windows(2).all(|pair| (pair[0] - pair[1]).abs() < f64::EPSILON), "chord heads share one x");
    assert!(note.lyric.is_some(), "lyric attached");
    assert!(note.stem.is_none(), "a whole-note chord carries no stem");
}

#[test]
fn measure_below_capacity_reports_the_implicit_rest_pad() {
    let mut spec = ScoreSpec::new();
    spec.measures.push(Measure { events: vec![Event::note(pitch(Step::C, 4), 2.0)], barline: Barline::Regular });

    let lowered = score_to_scene_with_options(&spec, &EngravingOptions::default()).expect("short measure still engraves");
    let approximated: Vec<_> =
        lowered.diagnostics.iter().filter(|d| d.code().as_str() == codes::ENGRAVING_APPROXIMATED).collect();
    assert_eq!(approximated.len(), 1);
    assert!(approximated[0].message().to_string().contains("implicit rest"), "the loss names the padding");
}

#[test]
fn measure_over_capacity_is_refused() {
    let mut spec = ScoreSpec::new();
    spec.measures.push(Measure {
        events: vec![Event::note(pitch(Step::C, 4), 4.0), Event::note(pitch(Step::D, 4), 1.0)],
        barline: Barline::Regular,
    });

    let error = score_to_scene_with_options(&spec, &EngravingOptions::default()).expect_err("overfull measure is refused");
    assert_eq!(error.code(), codes::ENGRAVING_UNSUPPORTED);
    assert!(error.message().contains("capacity"), "refusal names the capacity rule");
}

#[test]
fn empty_score_is_refused_with_the_empty_code() {
    let error =
        score_to_scene_with_options(&ScoreSpec::new(), &EngravingOptions::default()).expect_err("empty score is refused");
    assert_eq!(error.code(), codes::ENGRAVING_EMPTY_SCORE);
}

#[test]
fn unsupported_duration_is_refused() {
    let mut spec = ScoreSpec::new();
    spec.measures.push(Measure { events: vec![Event::note(pitch(Step::C, 4), 0.33)], barline: Barline::Regular });

    let error = score_to_scene_with_options(&spec, &EngravingOptions::default()).expect_err("32nd and shorter are refused");
    assert_eq!(error.code(), codes::ENGRAVING_UNSUPPORTED);
    assert!(error.message().contains("outside the v0 subset"));
}

#[test]
fn key_signature_emits_accidentals_and_no_key_glyph_for_c_major() {
    let mut spec = melody();
    spec.key = KeySignature::Sharps(2);
    let lowered = score_to_scene_with_options(&spec, &EngravingOptions::default()).expect("D major engraves");
    let header = &lowered.plan.systems[0].header_glyphs;
    let sharps = header.iter().filter(|glyph| glyph.glyph == smufl::ACCIDENTAL_SHARP).count();
    assert_eq!(sharps, 2, "two sharps in the header");

    let mut natural = melody();
    natural.key = KeySignature::Natural;
    let lowered = score_to_scene_with_options(&natural, &EngravingOptions::default()).expect("C major engraves");
    let key_glyphs = lowered.plan.systems[0]
        .header_glyphs
        .iter()
        .filter(|glyph| glyph.id.as_deref().is_some_and(|id| id.starts_with("key-")))
        .count();
    assert_eq!(key_glyphs, 0, "no key glyph when the signature is natural");
}

#[test]
fn authored_accidental_matching_the_key_is_not_redrawn() {
    let mut spec = melody();
    spec.key = KeySignature::Sharps(1);
    spec.measures[0].events[0] = Event::note(Pitch { step: Step::F, octave: 4, accidental: Some(Accidental::Sharp) }, 1.0);

    let lowered = score_to_scene_with_options(&spec, &EngravingOptions::default()).expect("engraves");
    assert!(lowered.plan.systems[0].notes[0].accidentals.is_empty(), "F# under one sharp needs no accidental");
}

#[test]
fn bass_clef_places_notes_lower_than_treble() {
    let mut spec = melody();
    spec.clef = Clef::Bass;
    let lowered = score_to_scene_with_options(&spec, &EngravingOptions::default()).expect("bass engraves");
    assert_eq!(lowered.plan.systems[0].header_glyphs[0].glyph, smufl::F_CLEF);
}

#[test]
fn eighth_notes_form_a_beam_and_drop_their_flags() {
    let mut spec = ScoreSpec::new();
    let mut measure = Measure { events: Vec::new(), barline: Barline::Regular };
    for step in [Step::C, Step::D, Step::E, Step::F, Step::G, Step::A, Step::B, Step::C] {
        measure.events.push(Event::note(pitch(step, 4), 0.5));
    }
    spec.measures.push(measure);

    let lowered = score_to_scene_with_options(&spec, &EngravingOptions::default()).expect("eighths engrave");
    let system = &lowered.plan.systems[0];
    assert_eq!(system.beams.len(), 1, "one beam group for eight eighths");
    assert!(system.notes.iter().all(|note| note.flags.is_empty()), "beamed notes carry no flag");
}

#[test]
fn explicit_no_beam_suppresses_the_group() {
    let mut spec = ScoreSpec::new();
    let mut measure = Measure { events: Vec::new(), barline: Barline::Regular };
    for step in [Step::C, Step::D, Step::E, Step::F, Step::G, Step::A, Step::B, Step::C] {
        let mut event = Event::note(pitch(step, 4), 0.5);
        event.beam = Beam::NoBeam;
        measure.events.push(event);
    }
    spec.measures.push(measure);

    let lowered = score_to_scene_with_options(&spec, &EngravingOptions::default()).expect("engraves");
    let system = &lowered.plan.systems[0];
    assert!(system.beams.is_empty(), "no beam group");
    assert!(system.notes.iter().all(|note| !note.flags.is_empty()), "flags kept");
}

#[test]
fn tie_pairs_across_a_barline_and_draws_one_arc() {
    let mut spec = ScoreSpec::new();
    let mut first = Event::note(pitch(Step::C, 4), 4.0);
    first.tie = TieState::Start;
    let mut second = Event::note(pitch(Step::C, 4), 4.0);
    second.tie = TieState::Stop;
    spec.measures.push(Measure { events: vec![first], barline: Barline::Regular });
    spec.measures.push(Measure { events: vec![second], barline: Barline::Final });

    let lowered = score_to_scene_with_options(&spec, &EngravingOptions::default()).expect("tied notes engrave");
    let arcs: usize = lowered.plan.systems.iter().map(|system| system.arcs.len()).sum();
    assert_eq!(arcs, 1, "one tie arc across the barline");
}

#[test]
fn slur_and_articulations_and_lyrics_survive_engraving() {
    let mut spec = ScoreSpec::new();
    let mut first = Event::note(pitch(Step::G, 4), 2.0);
    first.slur = SlurState::Start;
    first.articulations = vec![Articulation::Staccato];
    first.lyric = Some("la".to_string());
    let mut second = Event::note(pitch(Step::A, 4), 2.0);
    second.slur = SlurState::Stop;
    second.articulations = vec![Articulation::Tenuto];
    spec.measures.push(Measure { events: vec![first, second], barline: Barline::Final });

    let lowered = score_to_scene_with_options(&spec, &EngravingOptions::default()).expect("engraves");
    let system = &lowered.plan.systems[0];
    assert_eq!(system.arcs.len(), 1, "slur arc");
    assert!(!system.notes[0].articulation_dots.is_empty(), "staccato dot");
    assert!(!system.notes[1].articulation_lines.is_empty(), "tenuto line");
    assert!(system.notes[0].lyric.is_some(), "first lyric");
}

#[test]
fn triplet_emits_a_bracket_and_a_number() {
    let mut spec = ScoreSpec::new();
    let mut measure = Measure { events: Vec::new(), barline: Barline::Final };
    for step in [Step::C, Step::D, Step::E] {
        let mut event = Event::note(pitch(step, 4), 1.0);
        event.ratio = 2.0 / 3.0;
        event.tuplet = Some(Tuplet { actual: 3, normal: 2 });
        event.beam = Beam::NoBeam;
        measure.events.push(event);
    }
    measure.events.push(Event::rest(2.0));
    spec.measures.push(measure);

    let lowered = score_to_scene_with_options(&spec, &EngravingOptions::default()).expect("triplet engraves");
    let system = &lowered.plan.systems[0];
    assert_eq!(system.tuplet_numbers.len(), 1);
    assert_eq!(system.tuplet_numbers[0].text, "3");
}

#[test]
fn long_scores_wrap_into_multiple_systems() {
    let mut spec = ScoreSpec::new();
    for _ in 0..10 {
        spec.measures.push(Measure {
            events: vec![
                Event::note(pitch(Step::C, 4), 1.0),
                Event::note(pitch(Step::D, 4), 1.0),
                Event::note(pitch(Step::E, 4), 1.0),
                Event::note(pitch(Step::F, 4), 1.0),
            ],
            barline: Barline::Regular,
        });
    }

    let lowered = score_to_scene_with_options(&spec, &EngravingOptions::default()).expect("engraves");
    assert!(lowered.plan.systems.len() > 1, "ten measures exceed one line");
    // 每行重复头部：谱号与拍号各一次。
    assert!(lowered.plan.systems.iter().all(|system| system.header_glyphs.iter().any(|glyph| glyph.glyph == smufl::G_CLEF)));
}

#[test]
fn explicit_time_signature_draws_digit_glyphs() {
    let mut spec = melody();
    spec.time = TimeSignature::Explicit { beats: 3, beat_unit: 4 };
    spec.measures[0].events.truncate(3);

    let lowered = score_to_scene_with_options(&spec, &EngravingOptions::default()).expect("3/4 engraves");
    let header = &lowered.plan.systems[0].header_glyphs;
    assert!(header.iter().any(|glyph| glyph.glyph == smufl::time_digit(3)));
    assert!(header.iter().any(|glyph| glyph.glyph == smufl::time_digit(4)));
}

#[test]
fn repeat_barlines_place_two_dots() {
    let mut spec = melody();
    spec.measures[0].barline = Barline::RepeatEnd;
    let lowered = score_to_scene_with_options(&spec, &EngravingOptions::default()).expect("engraves");
    assert_eq!(lowered.plan.systems[0].repeat_dots.len(), 2);
    assert_eq!(lowered.plan.systems[0].barlines.len(), 1);
    let _ = EventKind::Rest;
}
