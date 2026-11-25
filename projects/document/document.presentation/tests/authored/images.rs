//! Image-object and asset-garbage-collection tests for the authored model.

use document_presentation::authored::{
    AssetId, Crop, Deck, DeckCreateOptions, DeckEdit, ElementContent, ElementId, ImageDuplicateOptions, ImageElementInput,
    ImageMoveTarget, MediaKind, SlideId, SlideAddOptions, TextContent, TextElementInput, Transform, codes,
};

fn deck() -> Deck {
    Deck::create(DeckCreateOptions::default())
}

fn transform() -> Transform {
    Transform::from_points(40.0, 120.0, 400.0, 200.0).expect("valid box")
}

/// A deck with one slide, two assets and one image element placed from the first.
fn deck_with_image() -> (Deck, SlideId, ElementId, AssetId, AssetId) {
    let mut deck = deck();
    let (slide, first, second, element) = deck
        .transaction(None, |edit| {
            let slide = edit.add_slide(SlideAddOptions::default())?;
            let first = edit.add_asset("a.png", "image/png", b"first-bytes".to_vec(), None)?;
            let second = edit.add_asset("b.png", "image/png", b"second-bytes".to_vec(), None)?;
            let element = edit.add_image(&slide, ImageElementInput { asset: first.clone(), transform: Some(transform()), alt: "图一".to_string() })?;
            Ok((slide, first, second, element))
        })
        .expect("commit");
    (deck, slide, element, first, second)
}

struct Placement {
    asset: AssetId,
    crop: Option<Crop>,
}

fn image_of(deck: &Deck, slide: &SlideId, element: &ElementId) -> Placement {
    let slide = deck.slide(slide).expect("slide");
    let target = slide.element(element).expect("element");
    let ElementContent::Image(image) = &target.content else {
        panic!("expected an image element");
    };
    Placement { asset: image.asset.clone(), crop: image.crop }
}

#[test]
fn replace_image_asset_swaps_only_the_asset() {
    let (mut deck, slide, element, first, second) = deck_with_image();
    let crop = Crop { left: 0.1, top: 0.0, right: 0.2, bottom: 0.3 };
    deck.transaction(None, |edit| edit.set_image_crop(&slide, &element, Some(crop))).expect("crop");
    let replaced = deck.transaction(None, |edit| edit.replace_image_asset(&slide, &element, second.clone())).expect("replace");
    assert_eq!(replaced, first);
    let state = image_of(&deck, &slide, &element);
    assert_eq!(state.asset, second);
    assert_eq!(state.crop, Some(crop));
    // The old asset survives until pruned; other placements are untouched.
    assert!(deck.asset(&first).is_some());
}

#[test]
fn image_operations_reject_wrong_kinds_and_unknown_assets() {
    let (mut deck, slide, element, first, second) = deck_with_image();
    let text = deck
        .transaction(None, |edit| {
            edit.add_text(&slide, TextElementInput { text: TextContent::plain("字"), transform: Some(transform()), alt: None, placeholder: None })
        })
        .expect("text");
    let error = deck
        .transaction::<()>(None, |edit| edit.replace_image_asset(&slide, &text, second).map(|_| ()))
        .expect_err("text is not an image");
    assert_eq!(error.code(), codes::OBJECT_TYPE_MISMATCH);
    let unknown = AssetId::new("asset-absent").expect("id");
    let error = deck
        .transaction::<()>(None, |edit| edit.replace_image_asset(&slide, &element, unknown).map(|_| ()))
        .expect_err("unknown asset");
    assert_eq!(error.code(), codes::ASSET_MISSING);
    // The failed transactions consumed no revision and kept the placement.
    assert_eq!(deck.revision(), 2);
    assert_eq!(image_of(&deck, &slide, &element).asset, first);
}

#[test]
fn crop_validation_rejects_crossing_fractions() {
    let (mut deck, slide, element, _, _) = deck_with_image();
    for crop in [
        Crop { left: 0.6, top: 0.0, right: 0.5, bottom: 0.0 },
        Crop { left: 0.0, top: 0.7, right: 0.0, bottom: 0.4 },
        Crop { left: -0.1, top: 0.0, right: 0.0, bottom: 0.0 },
        Crop { left: f64::NAN, top: 0.0, right: 0.0, bottom: 0.0 },
    ] {
        let error = deck
            .transaction::<()>(None, |edit| edit.set_image_crop(&slide, &element, Some(crop)))
            .expect_err("crossing crop must fail");
        assert_eq!(error.code(), codes::ARGUMENT_INVALID);
    }
    assert_eq!(deck.revision(), 1);
    let even = Crop { left: 0.25, top: 0.25, right: 0.25, bottom: 0.25 };
    deck.transaction(None, |edit| edit.set_image_crop(&slide, &element, Some(even))).expect("valid crop");
    assert_eq!(image_of(&deck, &slide, &element).crop, Some(even));
    deck.transaction(None, |edit| edit.set_image_crop(&slide, &element, None)).expect("clear crop");
    assert_eq!(image_of(&deck, &slide, &element).crop, None);
}

#[test]
fn duplicate_image_shares_the_asset_with_a_fresh_identity() {
    let (mut deck, slide, element, asset, _) = deck_with_image();
    let crop = Crop { left: 0.1, top: 0.1, right: 0.1, bottom: 0.1 };
    let copy = deck
        .transaction(None, |edit| {
            edit.set_image_crop(&slide, &element, Some(crop))?;
            edit.duplicate_image(&slide, &element, ImageDuplicateOptions { target_slide: None, offset_x_emu: Some(20_000), offset_y_emu: None })
        })
        .expect("duplicate");
    assert_ne!(copy, element);
    let state = image_of(&deck, &slide, &copy);
    assert_eq!(state.asset, asset);
    assert_eq!(state.crop, Some(crop));
    let source = deck.slide(&slide).expect("slide").element(&element).expect("source").to_owned();
    let placed = deck.slide(&slide).expect("slide").element(&copy).expect("copy").to_owned();
    assert_eq!(placed.transform.x_emu, source.transform.x_emu + 20_000);
    assert_eq!(placed.alt.as_deref(), Some("图一"));
    deck.document().validate().expect("identities stay unique");
}

#[test]
fn move_image_keeps_identity_across_slides() {
    let (mut deck, slide, element, _, _) = deck_with_image();
    let other = deck.transaction(None, |edit| edit.add_slide(SlideAddOptions::default())).expect("second slide");
    let index = deck
        .transaction(None, |edit| edit.move_image(&slide, &element, ImageMoveTarget { slide: other.clone(), index: Some(0) }))
        .expect("move");
    assert_eq!(index, 0);
    assert!(deck.slide(&slide).expect("source").elements.is_empty());
    assert_eq!(deck.slide(&other).expect("target").element(&element).expect("identity kept").id, element);
}

#[test]
fn asset_removal_respects_references_and_prunes_the_rest() {
    let (mut deck, slide, element, first, second) = deck_with_image();
    deck.transaction(None, |edit| {
        let other = edit.add_slide(SlideAddOptions::default())?;
        edit.add_media(&other, MediaKind::Audio, second.clone(), "旁白", None)?;
        Ok(())
    })
    .expect("media");
    // Both assets are placed: nothing is unused.
    let unused = deck.transaction(None, |edit: &mut DeckEdit| Ok(edit.unused_assets())).expect("unused");
    assert!(unused.is_empty());
    let error = deck.transaction::<()>(None, |edit| edit.remove_asset(&first)).expect_err("placed asset cannot be removed");
    assert_eq!(error.code(), codes::ASSET_IN_USE);
    assert_eq!(error.slide_id().expect("located slide"), &slide);
    assert_eq!(error.element_id().expect("located element"), &element);
    // Removing the placement first frees the asset.
    deck.transaction(None, |edit| {
        edit.remove_element(&slide, &element)?;
        edit.remove_asset(&first)
    })
    .expect("free first asset");
    assert!(deck.asset(&first).is_none());
    assert!(deck.asset(&second).is_some());
    // Add an orphan, then prune: only the orphan goes.
    let orphan = deck.transaction(None, |edit| edit.add_asset("c.png", "image/png", b"orphan".to_vec(), None)).expect("orphan");
    let pruned = deck.transaction(None, |edit| Ok(edit.prune_assets())).expect("prune");
    assert_eq!(pruned, vec![orphan.clone()]);
    assert!(deck.asset(&orphan).is_none());
    assert!(deck.asset(&second).is_some());
}

#[test]
fn failed_image_transaction_rolls_back_completely() {
    let (mut deck, slide, element, first, _) = deck_with_image();
    let result = deck.transaction::<()>(None, |edit| {
        edit.duplicate_image(&slide, &element, ImageDuplicateOptions::default())?;
        edit.remove_asset(&first) // still placed by the copy and the original
    });
    assert_eq!(result.expect_err("in-use asset").code(), codes::ASSET_IN_USE);
    assert_eq!(deck.revision(), 1);
    assert_eq!(deck.slide(&slide).expect("slide").elements.len(), 1);
    assert!(deck.asset(&first).is_some());
}
