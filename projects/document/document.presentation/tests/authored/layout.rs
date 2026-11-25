//! Element layout (alignment, distribution, z-order) tests for the authored model.

use document_presentation::authored::{
    AlignEdge, AlignReference, Deck, DeckCreateOptions, DistributeAxis, Element, ElementId, ElementZOrder, ShapeElementInput, SlideAddOptions,
    SlideId, Transform, codes, decode_project, encode_project,
};

fn deck() -> Deck {
    Deck::create(DeckCreateOptions::default())
}

fn emu(points: f64) -> i64 {
    (points * 12_700.0).round() as i64
}

fn frame(x: f64, y: f64, width: f64, height: f64) -> Transform {
    Transform::from_points(x, y, width, height).expect("valid box")
}

/// A deck with one slide and one shape per frame, in paint order.
fn shape_deck(frames: &[(f64, f64, f64, f64)]) -> (Deck, SlideId, Vec<ElementId>) {
    let mut deck = deck();
    let slide = deck.transaction(None, |edit| edit.add_slide(SlideAddOptions::default())).expect("slide");
    let ids = deck
        .transaction(None, |edit| {
            let mut ids = Vec::new();
            for (x, y, width, height) in frames {
                ids.push(edit.add_shape(
                    &slide,
                    ShapeElementInput {
                        geometry: "rect".to_string(),
                        transform: Some(frame(*x, *y, *width, *height)),
                        fill: None,
                        line: None,
                        alt: None,
                    },
                )?);
            }
            Ok(ids)
        })
        .expect("shapes");
    (deck, slide, ids)
}

fn placed(deck: &Deck, slide: &SlideId, ids: &[ElementId]) -> Vec<(ElementId, Transform)> {
    let model = deck.slide(slide).expect("slide");
    ids.iter().map(|id| (id.clone(), model.element(id).expect("element").transform)).collect()
}

fn order(deck: &Deck, slide: &SlideId) -> Vec<ElementId> {
    deck.slide(slide).expect("slide").elements.iter().map(|element: &Element| element.id.clone()).collect()
}

#[test]
fn align_pins_each_edge_to_the_slide() {
    let (mut deck, slide, ids) = shape_deck(&[(10.0, 10.0, 50.0, 50.0), (20.0, 20.0, 30.0, 40.0)]);
    let size = deck.document().size;
    let revision = deck.revision();

    for edge in [AlignEdge::Left, AlignEdge::Center, AlignEdge::Right, AlignEdge::Top, AlignEdge::Middle, AlignEdge::Bottom] {
        deck.transaction(None, |edit| edit.align_elements(&slide, &ids, edge, AlignReference::Slide)).expect("align");
        let model = deck.slide(&slide).expect("slide");
        for id in &ids {
            let transform = model.element(id).expect("element").transform;
            match edge {
                AlignEdge::Left => assert_eq!(transform.x_emu, 0),
                AlignEdge::Center => assert_eq!(transform.x_emu, (size.width_emu - transform.width_emu) / 2),
                AlignEdge::Right => assert_eq!(transform.x_emu, size.width_emu - transform.width_emu),
                AlignEdge::Top => assert_eq!(transform.y_emu, 0),
                AlignEdge::Middle => assert_eq!(transform.y_emu, (size.height_emu - transform.height_emu) / 2),
                AlignEdge::Bottom => assert_eq!(transform.y_emu, size.height_emu - transform.height_emu),
            }
        }
    }
    assert_eq!(deck.revision(), revision + 6, "each alignment is its own commit");
    assert_eq!(order(&deck, &slide), ids, "alignment never reorders the stack");
}

#[test]
fn align_to_selection_uses_the_joint_bounds() {
    let (mut deck, slide, ids) = shape_deck(&[(10.0, 10.0, 50.0, 50.0), (20.0, 20.0, 50.0, 50.0)]);
    deck.transaction(None, |edit| edit.align_elements(&slide, &ids, AlignEdge::Right, AlignReference::Selection)).expect("align");
    let frames = placed(&deck, &slide, &ids);
    assert_eq!(frames[0].1.x_emu, emu(20.0), "both boxes end at the selection's right edge");
    assert_eq!(frames[1].1.x_emu, emu(20.0));
    assert_eq!(frames[0].1.y_emu, emu(10.0), "the vertical axis stays untouched");
    assert_eq!(frames[1].1.y_emu, emu(20.0));

    deck.transaction(None, |edit| edit.align_elements(&slide, &ids, AlignEdge::Middle, AlignReference::Selection)).expect("align");
    let frames = placed(&deck, &slide, &ids);
    // The selection spans 10..70pt vertically; both 50pt boxes center at 15pt.
    assert_eq!(frames[0].1.y_emu, emu(15.0));
    assert_eq!(frames[1].1.y_emu, emu(15.0));
}

#[test]
fn distribute_spaces_the_middle_boxes_evenly() {
    let (mut deck, slide, ids) =
        shape_deck(&[(50.0, 10.0, 40.0, 40.0), (180.0, 10.0, 60.0, 40.0), (300.0, 10.0, 20.0, 40.0), (500.0, 10.0, 40.0, 40.0)]);
    deck.transaction(None, |edit| edit.distribute_elements(&slide, &ids, DistributeAxis::Horizontal)).expect("distribute");
    let frames = placed(&deck, &slide, &ids);
    assert_eq!(frames[0].1.x_emu, emu(50.0), "the outer boxes keep their positions");
    assert_eq!(frames[3].1.x_emu, emu(500.0));
    // Span 50..540, total width 160, three gaps of (490-160)/3 = 110.
    assert_eq!(frames[1].1.x_emu, emu(200.0));
    assert_eq!(frames[2].1.x_emu, emu(370.0));

    let (mut deck, slide, ids) =
        shape_deck(&[(10.0, 50.0, 40.0, 40.0), (10.0, 180.0, 40.0, 60.0), (10.0, 300.0, 40.0, 20.0), (10.0, 500.0, 40.0, 40.0)]);
    deck.transaction(None, |edit| edit.distribute_elements(&slide, &ids, DistributeAxis::Vertical)).expect("distribute");
    let frames = placed(&deck, &slide, &ids);
    assert_eq!(frames[1].1.y_emu, emu(200.0));
    assert_eq!(frames[2].1.y_emu, emu(370.0));
    assert_eq!(frames[0].1.x_emu, emu(10.0), "the horizontal axis stays untouched");
}

#[test]
fn distribute_ignores_the_list_order_and_sorts_by_position() {
    let (mut deck, slide, ids) =
        shape_deck(&[(50.0, 10.0, 40.0, 40.0), (180.0, 10.0, 60.0, 40.0), (300.0, 10.0, 20.0, 40.0), (500.0, 10.0, 40.0, 40.0)]);
    let shuffled = vec![ids[2].clone(), ids[0].clone(), ids[3].clone(), ids[1].clone()];
    deck.transaction(None, |edit| edit.distribute_elements(&slide, &shuffled, DistributeAxis::Horizontal)).expect("distribute");
    let frames = placed(&deck, &slide, &ids);
    assert_eq!(frames[1].1.x_emu, emu(200.0));
    assert_eq!(frames[2].1.x_emu, emu(370.0));
}

#[test]
fn z_order_moves_keep_identity_and_report_position() {
    let (mut deck, slide, ids) = shape_deck(&[(10.0, 10.0, 40.0, 40.0), (60.0, 10.0, 40.0, 40.0), (110.0, 10.0, 40.0, 40.0)]);
    let (first, second, third) = (ids[0].clone(), ids[1].clone(), ids[2].clone());

    let index = deck.transaction(None, |edit| edit.move_element_to(&slide, &second, 0)).expect("reorder");
    assert_eq!(index, 0);
    assert_eq!(order(&deck, &slide), vec![second.clone(), first.clone(), third.clone()]);

    let index = deck.transaction(None, |edit| edit.move_element_to(&slide, &first, 9)).expect("clamped reorder");
    assert_eq!(index, 2, "an index beyond the stack places the element last");
    assert_eq!(order(&deck, &slide), vec![second.clone(), third.clone(), first.clone()]);

    let index = deck.transaction(None, |edit| edit.set_element_z_order(&slide, &second, ElementZOrder::Front)).expect("front");
    assert_eq!(index, 2);
    assert_eq!(order(&deck, &slide), vec![third.clone(), first.clone(), second.clone()]);

    let index = deck.transaction(None, |edit| edit.set_element_z_order(&slide, &second, ElementZOrder::Back)).expect("back");
    assert_eq!(index, 0);
    assert_eq!(order(&deck, &slide), vec![second.clone(), third.clone(), first.clone()]);

    let index = deck.transaction(None, |edit| edit.set_element_z_order(&slide, &first, ElementZOrder::Forward)).expect("forward at the top");
    assert_eq!(index, 2, "a step off the top of the stack stays put");
    assert_eq!(order(&deck, &slide), vec![second.clone(), third.clone(), first.clone()]);
    let index = deck.transaction(None, |edit| edit.set_element_z_order(&slide, &third, ElementZOrder::Forward)).expect("forward");
    assert_eq!(index, 2);
    assert_eq!(order(&deck, &slide), vec![second.clone(), first.clone(), third.clone()]);
    let index = deck.transaction(None, |edit| edit.set_element_z_order(&slide, &first, ElementZOrder::Backward)).expect("backward");
    assert_eq!(index, 0);
    assert_eq!(order(&deck, &slide), vec![first.clone(), second.clone(), third.clone()]);
    deck.document().validate().expect("identities stay unique");
}

#[test]
fn layout_operations_reject_bad_arguments_without_touching_the_draft() {
    let (mut deck, slide, ids) = shape_deck(&[(10.0, 10.0, 50.0, 50.0), (20.0, 20.0, 50.0, 50.0)]);
    let revision = deck.revision();
    let before = placed(&deck, &slide, &ids);
    let unknown = ElementId::new("element-absent").expect("id");
    let missing = SlideId::new("slide-absent").expect("id");

    let error = deck
        .transaction::<()>(None, |edit| edit.align_elements(&slide, &[], AlignEdge::Left, AlignReference::Slide))
        .expect_err("an empty selection");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);
    let error = deck
        .transaction::<()>(None, |edit| edit.align_elements(&slide, &[ids[0].clone(), ids[0].clone()], AlignEdge::Left, AlignReference::Slide))
        .expect_err("a repeated element");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);
    let error = deck
        .transaction::<()>(None, |edit| edit.align_elements(&slide, &[unknown.clone()], AlignEdge::Left, AlignReference::Slide))
        .expect_err("an unknown element");
    assert_eq!(error.code(), codes::OBJECT_NOT_FOUND);
    let error = deck
        .transaction::<()>(None, |edit| edit.align_elements(&missing, &ids, AlignEdge::Left, AlignReference::Slide))
        .expect_err("an unknown slide");
    assert_eq!(error.code(), codes::OBJECT_NOT_FOUND);
    let error = deck
        .transaction::<()>(None, |edit| edit.distribute_elements(&slide, &ids, DistributeAxis::Horizontal))
        .expect_err("two boxes cannot be distributed");
    assert_eq!(error.code(), codes::ARGUMENT_INVALID);
    let error = deck.transaction::<usize>(None, |edit| edit.move_element_to(&slide, &unknown, 0)).expect_err("an unknown element");
    assert_eq!(error.code(), codes::OBJECT_NOT_FOUND);
    let error = deck
        .transaction::<usize>(None, |edit| edit.set_element_z_order(&slide, &unknown, ElementZOrder::Front))
        .expect_err("an unknown element");
    assert_eq!(error.code(), codes::OBJECT_NOT_FOUND);

    assert_eq!(deck.revision(), revision, "failed layout operations consume no revision");
    assert_eq!(placed(&deck, &slide, &ids), before);
}

#[test]
fn failed_layout_transaction_rolls_back_completely() {
    let (mut deck, slide, ids) = shape_deck(&[(50.0, 10.0, 40.0, 40.0), (180.0, 10.0, 60.0, 40.0), (300.0, 10.0, 20.0, 40.0)]);
    let revision = deck.revision();
    let before = placed(&deck, &slide, &ids);
    let unknown = ElementId::new("element-absent").expect("id");
    let result = deck.transaction::<()>(None, |edit| {
        edit.align_elements(&slide, &ids, AlignEdge::Left, AlignReference::Slide)?;
        edit.distribute_elements(&slide, &[ids[0].clone(), ids[1].clone(), unknown], DistributeAxis::Horizontal)
    });
    assert_eq!(result.expect_err("unknown element").code(), codes::OBJECT_NOT_FOUND);
    assert_eq!(deck.revision(), revision);
    assert_eq!(placed(&deck, &slide, &ids), before, "the alignment rolled back with the draft");
}

#[test]
fn layout_edits_survive_project_roundtrip() {
    let (mut deck, slide, ids) = shape_deck(&[(10.0, 10.0, 50.0, 50.0), (20.0, 20.0, 50.0, 50.0), (5.0, 40.0, 50.0, 50.0)]);
    deck.transaction(None, |edit| {
        edit.align_elements(&slide, &ids, AlignEdge::Center, AlignReference::Selection)?;
        edit.set_element_z_order(&slide, &ids[2], ElementZOrder::Front)
    })
    .expect("layout");

    let bytes = encode_project(&deck).expect("encode");
    let reopened = decode_project(&bytes).expect("decode");
    assert_eq!(reopened.revision(), deck.revision());
    assert_eq!(order(&reopened, &slide), order(&deck, &slide));
    assert_eq!(placed(&reopened, &slide, &ids), placed(&deck, &slide, &ids));
    assert_eq!(encode_project(&reopened).expect("re-encode"), bytes);
}
