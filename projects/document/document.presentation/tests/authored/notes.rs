//! Speaker-notes tests for the authored model.

use document_presentation::authored::{Deck, DeckCreateOptions, SlideAddOptions, codes};

fn deck() -> Deck {
    Deck::create(DeckCreateOptions::default())
}

#[test]
fn notes_outline_follows_playback_order_and_flags_hidden() {
    let mut deck = deck();
    assert!(deck.notes_outline().is_empty());
    let (first, second) = deck
        .transaction(None, |edit| {
            let first = edit.add_slide(SlideAddOptions::default())?;
            let second = edit.add_slide(SlideAddOptions::default())?;
            edit.set_slide_title(&first, "开场")?;
            edit.set_slide_notes(&first, Some("问候观众".to_string()))?;
            edit.set_slide_hidden(&second, true)?;
            Ok((first, second))
        })
        .expect("slides");
    let outline = deck.notes_outline();
    assert_eq!(outline.len(), 2);
    assert_eq!(outline[0].slide, first);
    assert_eq!(outline[0].index, 0);
    assert_eq!(outline[0].title, "开场");
    assert_eq!(outline[0].notes.as_deref(), Some("问候观众"));
    assert!(!outline[0].hidden);
    assert_eq!(outline[1].slide, second);
    assert!(outline[1].hidden);
    assert_eq!(outline[1].notes, None);
}

#[test]
fn append_and_clear_notes() {
    let mut deck = deck();
    let slide = deck.transaction(None, |edit| edit.add_slide(SlideAddOptions::default())).expect("slide");
    deck.transaction(None, |edit| edit.append_slide_notes(&slide, "第一段")).expect("first append");
    assert_eq!(deck.slide(&slide).expect("slide").notes.as_deref(), Some("第一段"));
    deck.transaction(None, |edit| edit.append_slide_notes(&slide, "第二段")).expect("second append");
    assert_eq!(deck.slide(&slide).expect("slide").notes.as_deref(), Some("第一段\n第二段"));
    let touched = deck
        .transaction(None, |edit| {
            edit.add_slide(SlideAddOptions::default())?;
            edit.append_notes_to_all("提醒：看时间")
        })
        .expect("append all");
    assert_eq!(touched, 2);
    assert_eq!(deck.slide(&slide).expect("slide").notes.as_deref(), Some("第一段\n第二段\n提醒：看时间"));
    deck.transaction(None, |edit| edit.clear_slide_notes(&slide)).expect("clear");
    assert_eq!(deck.slide(&slide).expect("slide").notes, None);
}

#[test]
fn blank_notes_are_rejected_without_consuming_a_revision() {
    let mut deck = deck();
    let slide = deck.transaction(None, |edit| edit.add_slide(SlideAddOptions::default())).expect("slide");
    let before = deck.revision();
    let error = deck.transaction::<()>(None, |edit| edit.append_slide_notes(&slide, "   ")).expect_err("blank text");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);
    let error = deck.transaction(None, |edit| edit.append_notes_to_all("")).expect_err("blank global text");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);
    assert_eq!(deck.revision(), before);
    assert_eq!(deck.slide(&slide).expect("slide").notes, None);
}
