//! Typed image-object and asset operations on a [`DeckEdit`] draft.
//!
//! These methods carry the image editing semantics the OpenPPT product exposed
//! through JSON script operations (`image.replace`, `image.setCrop`,
//! `image.duplicate`, `image.move`, `asset.remove`, `asset.prune`), but with
//! typed Rust inputs and outputs. Adding an image element itself stays on
//! [`DeckEdit::add_image`](super::edit::DeckEdit::add_image) and embedding
//! bytes stays on [`DeckEdit::add_asset`](super::edit::DeckEdit::add_asset).
//!
//! Every operation mutates the draft in place; atomicity and rollback come from
//! [`Deck::transaction`](super::model::Deck::transaction). Object identities are
//! preserved by all in-place edits; only [`DeckEdit::duplicate_image`] allocates
//! a fresh identity. Duplicated images share one asset record, so swapping the
//! bytes of a picture updates every placement at once.

use super::{
    edit::DeckEdit,
    error::{DeckError, codes},
    id::{AssetId, ElementId, SlideId},
    model::{Crop, Element, ElementContent, ElementOrigin, OriginKind},
};

/// Destination accepted by [`DeckEdit::move_image`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageMoveTarget {
    /// Slide receiving the image; may be the current slide for a reorder.
    pub slide: SlideId,
    /// Paint-order position inside the target slide after the image is detached;
    /// the image is appended when absent.
    pub index: Option<usize>,
}

/// Options accepted by [`DeckEdit::duplicate_image`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageDuplicateOptions {
    /// Slide receiving the copy; the source slide is used when absent.
    pub target_slide: Option<SlideId>,
    /// Extra left offset in EMU applied to the copy.
    pub offset_x_emu: Option<i64>,
    /// Extra top offset in EMU applied to the copy.
    pub offset_y_emu: Option<i64>,
}

impl DeckEdit {
    /// Points an image element at another asset, keeping crop, placement and alt.
    ///
    /// Returns the asset that was replaced. The new asset must already carry
    /// bytes; nothing is removed from the deck, so the old asset stays available
    /// for other placements until it is pruned.
    pub fn replace_image_asset(&mut self, slide: &SlideId, element: &ElementId, asset: AssetId) -> Result<AssetId, DeckError> {
        self.assert_asset(&asset)?;
        let target = self.image_mut(slide, element, "only images carry an asset placement")?;
        Ok(std::mem::replace(&mut target.asset, asset))
    }

    /// Sets or clears the crop of an image element.
    ///
    /// Every fraction must be finite and within `0..=1`, and the two horizontal
    /// and two vertical fractions must each leave visible extent.
    pub fn set_image_crop(&mut self, slide: &SlideId, element: &ElementId, crop: Option<Crop>) -> Result<(), DeckError> {
        if let Some(crop) = &crop {
            check_crop(crop)?;
        }
        self.image_mut(slide, element, "only images can be cropped")?.crop = crop;
        Ok(())
    }

    /// Copies an image with a fresh element identity onto the target slide
    /// (the source slide by default), optionally offset in EMU. The copy shares
    /// the asset record.
    pub fn duplicate_image(&mut self, slide: &SlideId, element: &ElementId, options: ImageDuplicateOptions) -> Result<ElementId, DeckError> {
        let source = {
            let model = self.document().slide(slide).ok_or_else(|| missing_slide(slide))?;
            model.element(element).cloned().ok_or_else(|| missing_element(slide, element))?
        };
        let ElementContent::Image(image) = &source.content else {
            return Err(not_an_image(slide, element, "only images can be duplicated as images"));
        };
        let mut transform = source.transform;
        transform.x_emu = transform.x_emu.saturating_add(options.offset_x_emu.unwrap_or(0));
        transform.y_emu = transform.y_emu.saturating_add(options.offset_y_emu.unwrap_or(0));
        let mut copy = Element::new(
            ElementId::new(self.allocate("element")).expect("allocated identity is non-empty"),
            transform,
            ElementContent::Image(image.clone()),
        );
        copy.alt = source.alt.clone();
        copy.lock = source.lock.clone();
        copy.source = Some(ElementOrigin { kind: OriginKind::Authoring, part: None });
        let copy_id = copy.id.clone();
        let destination = self.slide_mut(options.target_slide.as_ref().unwrap_or(slide))?;
        destination.elements.push(copy);
        Ok(copy_id)
    }

    /// Moves an image to another slide or to a new paint-order position on the
    /// same slide, keeping its identity. Returns the final position.
    pub fn move_image(&mut self, slide: &SlideId, element: &ElementId, target: ImageMoveTarget) -> Result<usize, DeckError> {
        assert_image(self.document().slide(slide), slide, element, "only images can be moved as images")?;
        let moved = {
            let source = self.slide_mut(slide)?;
            let index = source
                .elements
                .iter()
                .position(|candidate| &candidate.id == element)
                .ok_or_else(|| missing_element(slide, element))?;
            source.elements.remove(index)
        };
        let destination = self.slide_mut(&target.slide)?;
        let index = target.index.unwrap_or(destination.elements.len()).min(destination.elements.len());
        destination.elements.insert(index, moved);
        Ok(index)
    }

    /// Removes an unreferenced asset record with its bytes.
    ///
    /// While any image or media element still points at the asset the removal is
    /// rejected with [`codes::ASSET_IN_USE`], so a deck never holds a dangling
    /// reference.
    pub fn remove_asset(&mut self, asset: &AssetId) -> Result<(), DeckError> {
        if !self.document().assets.iter().any(|candidate| &candidate.id == asset) {
            return Err(DeckError::new(codes::ASSET_MISSING, format!("asset `{asset}` does not exist")).with_recoverable(true));
        }
        if let Some((slide, element)) = referencing(self.document(), asset) {
            return Err(DeckError::new(
                codes::ASSET_IN_USE,
                format!("asset `{asset}` is still placed on slide `{slide}` by element `{element}`"),
            )
            .with_slide(slide)
            .with_element(element));
        }
        let index = self.document().assets.iter().position(|candidate| &candidate.id == asset).expect("existence was checked");
        self.document_mut().assets.remove(index);
        Ok(())
    }

    /// Assets no image or media element references, in declaration order.
    pub fn unused_assets(&self) -> Vec<AssetId> {
        self.document()
            .assets
            .iter()
            .filter(|asset| referencing(self.document(), &asset.id).is_none())
            .map(|asset| asset.id.clone())
            .collect()
    }

    /// Removes every unreferenced asset and returns the removed identities.
    pub fn prune_assets(&mut self) -> Vec<AssetId> {
        let unused = self.unused_assets();
        if !unused.is_empty() {
            let document = self.document_mut();
            document.assets.retain(|asset| !unused.contains(&asset.id));
        }
        unused
    }

    /// Borrows the image content of one element, rejecting non-image targets.
    fn image_mut(&mut self, slide: &SlideId, element: &ElementId, needs: &str) -> Result<&mut super::model::ImageContent, DeckError> {
        let model = self.slide_mut(slide)?;
        let target = model.element_mut(element).ok_or_else(|| missing_element(slide, element))?;
        match &mut target.content {
            ElementContent::Image(image) => Ok(image),
            _ => Err(not_an_image(slide, element, needs)),
        }
    }
}

/// The first slide/element pair still placing `asset`, if any.
fn referencing(document: &super::model::DeckDocument, asset: &AssetId) -> Option<(SlideId, ElementId)> {
    for slide in &document.slides {
        for element in &slide.elements {
            let referenced = match &element.content {
                ElementContent::Image(image) => &image.asset,
                ElementContent::Media(media) => &media.asset,
                _ => continue,
            };
            if referenced == asset {
                return Some((slide.id.clone(), element.id.clone()));
            }
        }
    }
    None
}

fn assert_image(model: Option<&super::model::DeckSlide>, slide: &SlideId, element: &ElementId, needs: &str) -> Result<(), DeckError> {
    let model = model.ok_or_else(|| missing_slide(slide))?;
    let target = model.element(element).ok_or_else(|| missing_element(slide, element))?;
    match &target.content {
        ElementContent::Image(_) => Ok(()),
        _ => Err(not_an_image(slide, element, needs)),
    }
}

fn check_crop(crop: &Crop) -> Result<(), DeckError> {
    for value in [crop.left, crop.top, crop.right, crop.bottom] {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(invalid("crop fractions must be finite and between 0 and 1"));
        }
    }
    if crop.left + crop.right >= 1.0 {
        return Err(invalid("the horizontal crop fractions must leave visible width"));
    }
    if crop.top + crop.bottom >= 1.0 {
        return Err(invalid("the vertical crop fractions must leave visible height"));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> DeckError {
    DeckError::new(codes::ARGUMENT_INVALID, message)
}

fn missing_slide(slide: &SlideId) -> DeckError {
    DeckError::new(codes::OBJECT_NOT_FOUND, format!("slide `{slide}` does not exist")).with_slide(slide.clone())
}

fn missing_element(slide: &SlideId, element: &ElementId) -> DeckError {
    DeckError::new(codes::OBJECT_NOT_FOUND, format!("element `{element}` does not exist on this slide"))
        .with_slide(slide.clone())
        .with_element(element.clone())
}

fn not_an_image(slide: &SlideId, element: &ElementId, needs: &str) -> DeckError {
    DeckError::new(codes::OBJECT_TYPE_MISMATCH, format!("element `{element}` is not an image; {needs}"))
        .with_slide(slide.clone())
        .with_element(element.clone())
}
