//! Smoke tests for the authored deck model.
//!
//! They cover the closed loop required by the first editing gate: create, edit
//! atomically, save, reopen, play back statically and inspect. Corpus, visual
//! baselines and independent round trips belong to the OpenPPT product, not here.

use document_presentation::authored::{
    ChartData, ChartElementInput, ChartSeries, ChartType, Deck, DeckCreateOptions, DeckError, InspectCheck, InspectOptions, MediaKind,
    Severity, ShapeElementInput, SlideAddOptions, TextContent, TextElementInput, Transform, codes, decode_project, encode_project,
};

fn empty_deck() -> Deck {
    Deck::create(DeckCreateOptions::default())
}

fn body_transform() -> Transform {
    Transform::from_points(40.0, 120.0, 400.0, 200.0).expect("valid box")
}

#[test]
fn create_edit_save_and_reopen_keeps_the_loop() {
    let mut deck = empty_deck();
    assert_eq!(deck.revision(), 0);
    assert!(deck.layouts().len() >= 2);
    let layout = deck.layouts()[1].id.clone();

    let slide = deck
        .transaction(None, |edit| {
            let slide = edit.add_slide(SlideAddOptions { layout: Some(layout.clone()), index: None })?;
            edit.set_slide_title(&slide, "增长来自哪些业务")?;
            Ok(slide)
        })
        .expect("commit slide");
    assert_eq!(deck.revision(), 1);

    deck.transaction(None, |edit| {
        edit.add_chart(
            &slide,
            ChartElementInput {
                chart_type: ChartType::Bar,
                data: ChartData {
                    categories: vec!["订阅".to_string(), "广告".to_string()],
                    series: vec![ChartSeries { name: "收入".to_string(), values: vec![12.0, 8.0] }],
                },
                transform: None,
                alt: "各业务收入对比".to_string(),
            },
        )?;
        Ok(())
    })
    .expect("commit chart");
    assert_eq!(deck.revision(), 2);
    assert_eq!(deck.slides()[0].title(), "增长来自哪些业务");

    let bytes = encode_project(&deck).expect("encode project");
    let reopened = decode_project(&bytes).expect("decode project");
    assert_eq!(reopened.revision(), deck.revision());
    assert_eq!(reopened.slides()[0].title(), "增长来自哪些业务");
    assert_eq!(reopened.slides()[0].elements.len(), 2);
    assert_eq!(encode_project(&reopened).expect("re-encode"), bytes);
}

#[test]
fn failed_transaction_leaves_no_half_slide_and_no_revision() {
    let mut deck = empty_deck();
    let result = deck.transaction(None, |edit| {
        edit.add_slide(SlideAddOptions::default())?;
        Err::<(), DeckError>(DeckError::new(codes::ARGUMENT_INVALID, "abort"))
    });
    assert!(result.is_err());
    assert_eq!(deck.revision(), 0);
    assert!(deck.slides().is_empty());
    assert_eq!(deck.undo_depth(), 0);
}

#[test]
fn stale_revision_is_rejected() {
    let mut deck = empty_deck();
    deck.transaction(None, |edit| edit.add_slide(SlideAddOptions::default())).expect("first commit");
    let error = deck.transaction(Some(0), |edit| edit.add_slide(SlideAddOptions::default())).expect_err("stale revision must be rejected");
    assert_eq!(error.code(), codes::DECK_REVISION_CONFLICT);
    assert!(error.recoverable());
    assert_eq!(deck.slides().len(), 1);
}

#[test]
fn undo_and_redo_replay_commits() {
    let mut deck = empty_deck();
    deck.transaction(None, |edit| edit.add_slide(SlideAddOptions::default())).expect("commit");
    assert!(deck.undo());
    assert!(deck.slides().is_empty());
    assert_eq!(deck.revision(), 2);
    assert!(deck.redo());
    assert_eq!(deck.slides().len(), 1);
    assert_eq!(deck.revision(), 3);
    assert!(!deck.redo());
}

#[test]
fn hidden_slides_are_skipped_by_static_playback() {
    let mut deck = empty_deck();
    let (first, second) = deck
        .transaction(None, |edit| {
            let first = edit.add_slide(SlideAddOptions::default())?;
            let second = edit.add_slide(SlideAddOptions::default())?;
            edit.set_slide_hidden(&second, true)?;
            edit.set_slide_notes(&first, Some("开场".to_string()))?;
            Ok((first, second))
        })
        .expect("commit slides");

    let mut session = deck.playback(None).expect("session");
    assert_eq!(session.slides(), std::slice::from_ref(&first));
    session.start().expect("start");
    assert_eq!(session.state().slide_id.as_ref(), Some(&first));
    assert_eq!(session.notes(), Some("开场"));
    session.next().expect("next stops at the end");
    assert!(!session.state().playing);
    assert!(deck.playback(Some(&second)).is_err());
}

#[test]
fn inspect_reports_out_of_bounds_contrast_and_missing_checks() {
    let mut deck = empty_deck();
    deck.transaction(None, |edit| {
        let slide = edit.add_slide(SlideAddOptions::default())?;
        let mut outside = body_transform();
        outside.x_emu = -1;
        let mut text = TextContent::plain("对比度不足");
        text.paragraphs[0].runs[0].color = Some("#777777".to_string());
        edit.add_text(&slide, TextElementInput { text, transform: Some(outside), alt: None, placeholder: None })?;
        edit.add_shape(&slide, ShapeElementInput { geometry: "rect".to_string(), transform: None, fill: None, line: None, alt: None })?;
        Ok(())
    })
    .expect("commit");

    let report = deck.inspect(InspectOptions { checks: Some(vec![InspectCheck::OutOfBounds, InspectCheck::Contrast, InspectCheck::Overflow]) });
    assert!(report.issues.iter().any(|issue| issue.code == "deck.out-of-bounds"));
    assert!(report.issues.iter().any(|issue| issue.code == "deck.check-unsupported"));
    assert!(report.passed, "warnings do not fail the run");
    assert!(report.issues.iter().all(|issue| issue.severity != Severity::Error));
}

#[test]
fn duplicated_slides_receive_fresh_identities() {
    let mut deck = empty_deck();
    let (original, copy) = deck
        .transaction(None, |edit| {
            let slide = edit.add_slide(SlideAddOptions::default())?;
            edit.add_text(
                &slide,
                TextElementInput { text: TextContent::plain("hello"), transform: Some(body_transform()), alt: None, placeholder: None },
            )?;
            let copy = edit.duplicate_slide(&slide, None)?;
            Ok((slide, copy))
        })
        .expect("duplicate");

    assert_ne!(original, copy);
    let original_elements: Vec<_> = deck.slides()[0].elements.iter().map(|element| element.id.clone()).collect();
    let copy_elements: Vec<_> = deck.slides()[1].elements.iter().map(|element| element.id.clone()).collect();
    assert_eq!(original_elements.len(), copy_elements.len());
    assert!(original_elements.iter().all(|id| !copy_elements.contains(id)));
    deck.document().validate().expect("identities stay unique");
}

mod deck_style;
mod error_params;
mod images;
mod layout;
mod notes;
mod slides;
mod tables;
mod text_runs;

#[test]
fn media_elements_keep_asset_identity_across_reopen() {
    let mut deck = empty_deck();
    let asset = deck.transaction(None, |edit| edit.add_asset("clip.mp4", "video/mp4", b"video-bytes".to_vec(), None)).expect("asset");
    assert!(deck.asset(&asset).is_some());

    let bytes = encode_project(&deck).expect("encode");
    let mut reopened = decode_project(&bytes).expect("decode");
    assert_eq!(reopened.asset(&asset).expect("asset survives reopen").bytes, b"video-bytes");

    reopened
        .transaction(None, |edit| {
            let slide = edit.add_slide(SlideAddOptions::default())?;
            edit.add_media(&slide, MediaKind::Video, asset, "intro", None)?;
            Ok(())
        })
        .expect("media");
    assert!(reopened.inspect(InspectOptions { checks: Some(vec![InspectCheck::Assets]) }).passed);
}
