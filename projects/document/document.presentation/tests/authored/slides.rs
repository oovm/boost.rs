//! Slide-level structural edits (move, swap, layout, background) tests.

use document_presentation::authored::{
    Deck, DeckCreateOptions, ElementId, Fill, LayoutId, SlideAddOptions, SlideId, TextContent, TextElementInput, Transform, codes,
    decode_project, encode_project,
};

fn deck() -> Deck {
    Deck::create(DeckCreateOptions::default())
}

fn frame() -> Transform {
    Transform::from_points(40.0, 120.0, 400.0, 200.0).expect("valid box")
}

/// A deck with `count` slides, each carrying one text element.
fn slide_deck(count: usize) -> (Deck, Vec<SlideId>, Vec<ElementId>) {
    let mut deck = deck();
    let slides = (0..count)
        .map(|_| deck.transaction(None, |edit| edit.add_slide(SlideAddOptions::default())).expect("slide"))
        .collect::<Vec<_>>();
    let elements = deck
        .transaction(None, |edit| {
            slides
                .iter()
                .map(|slide| {
                    edit.add_text(
                        slide,
                        TextElementInput { text: TextContent::plain("body"), transform: Some(frame()), alt: None, placeholder: None },
                    )
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .expect("elements");
    (deck, slides, elements)
}

fn order(deck: &Deck) -> Vec<SlideId> {
    deck.slides().iter().map(|slide| slide.id.clone()).collect()
}

fn element_ids(deck: &Deck) -> Vec<ElementId> {
    deck.slides().iter().flat_map(|slide| slide.elements.iter().map(|element| element.id.clone())).collect()
}

#[test]
fn move_and_swap_slides_keep_every_identity() {
    let (mut deck, slides, elements) = slide_deck(3);
    let (first, second, third) = (slides[0].clone(), slides[1].clone(), slides[2].clone());

    let index = deck.transaction(None, |edit| edit.move_slide(&third, 0)).expect("move to the front");
    assert_eq!(index, 0);
    assert_eq!(order(&deck), vec![third.clone(), first.clone(), second.clone()]);

    let index = deck.transaction(None, |edit| edit.move_slide(&first, 9)).expect("clamped move");
    assert_eq!(index, 2, "an index beyond the deck places the slide last");
    assert_eq!(order(&deck), vec![third.clone(), second.clone(), first.clone()]);

    deck.transaction(None, |edit| edit.swap_slides(&third, &first)).expect("swap");
    assert_eq!(order(&deck), vec![first.clone(), second.clone(), third.clone()]);

    deck.transaction(None, |edit| edit.swap_slides(&first, &first)).expect("self swap");
    assert_eq!(order(&deck), vec![first.clone(), second.clone(), third.clone()]);

    assert_eq!(element_ids(&deck), elements, "structural moves never re-issue element identities");
    deck.document().validate().expect("identities stay unique");
}

#[test]
fn slide_layout_and_background_survive_project_roundtrip() {
    let (mut deck, slides, _) = slide_deck(1);
    let slide = slides[0].clone();
    let layout = deck.layouts()[1].id.clone();
    let fill = Fill::Solid { color: "#0F172A".to_string() };

    deck.transaction(None, |edit| {
        edit.set_slide_layout(&slide, Some(layout.clone()))?;
        edit.set_slide_background(&slide, Some(fill.clone()))
    })
    .expect("assign layout and background");
    let model = deck.slide(&slide).expect("slide");
    assert_eq!(model.layout.as_ref(), Some(&layout));
    assert_eq!(model.background.as_ref(), Some(&fill));

    let bytes = encode_project(&deck).expect("encode");
    let mut reopened = decode_project(&bytes).expect("decode");
    let model = reopened.slide(&slide).expect("slide survives reopen");
    assert_eq!(model.layout.as_ref(), Some(&layout));
    assert_eq!(model.background.as_ref(), Some(&fill));
    assert_eq!(encode_project(&reopened).expect("re-encode"), bytes);

    reopened
        .transaction(None, |edit| {
            edit.set_slide_layout(&slide, None)?;
            edit.set_slide_background(&slide, None)
        })
        .expect("clear layout and background");
    let model = reopened.slide(&slide).expect("slide");
    assert_eq!(model.layout, None);
    assert_eq!(model.background, None);
}

#[test]
fn slide_operations_reject_bad_arguments_without_touching_the_draft() {
    let (mut deck, slides, _) = slide_deck(3);
    let revision = deck.revision();
    let before = order(&deck);
    let missing = SlideId::new("slide-absent").expect("id");
    let unknown_layout = LayoutId::new("layout-absent").expect("id");

    let error = deck.transaction::<usize>(None, |edit| edit.move_slide(&missing, 0)).expect_err("an unknown slide");
    assert_eq!(error.code(), codes::OBJECT_NOT_FOUND);
    let error = deck.transaction::<()>(None, |edit| edit.swap_slides(&slides[0], &missing)).expect_err("an unknown slide");
    assert_eq!(error.code(), codes::OBJECT_NOT_FOUND);
    let error = deck
        .transaction::<()>(None, |edit| edit.set_slide_layout(&missing, Some(unknown_layout.clone())))
        .expect_err("an unknown slide");
    assert_eq!(error.code(), codes::OBJECT_NOT_FOUND);
    let error =
        deck.transaction::<()>(None, |edit| edit.set_slide_layout(&slides[0], Some(unknown_layout))).expect_err("an unknown layout");
    assert_eq!(error.code(), codes::OBJECT_NOT_FOUND);
    let error = deck
        .transaction::<()>(None, |edit| edit.set_slide_background(&missing, Some(Fill::None)))
        .expect_err("an unknown slide");
    assert_eq!(error.code(), codes::OBJECT_NOT_FOUND);

    assert_eq!(deck.revision(), revision, "failed slide operations consume no revision");
    assert_eq!(order(&deck), before);
}

#[test]
fn failed_slide_transaction_rolls_back_completely() {
    let (mut deck, slides, _) = slide_deck(3);
    let revision = deck.revision();
    let before = order(&deck);
    let missing = SlideId::new("slide-absent").expect("id");
    let result = deck.transaction::<()>(None, |edit| {
        edit.move_slide(&slides[2], 0)?;
        edit.swap_slides(&slides[0], &missing)
    });
    assert_eq!(result.expect_err("unknown slide").code(), codes::OBJECT_NOT_FOUND);
    assert_eq!(deck.revision(), revision);
    assert_eq!(order(&deck), before, "the move rolled back with the draft");
}
