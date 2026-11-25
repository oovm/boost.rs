//! Element layout operations on a [`DeckEdit`] draft.
//!
//! These methods carry the alignment, distribution and paint-order (z-order)
//! semantics the OpenPPT product exposed through JSON script operations
//! (`element.align`, `element.distribute`, `element.reorder`,
//! `element.bringToFront`, `element.sendToBack`, `element.bringForward`,
//! `element.sendBackward`), but with typed Rust inputs. Setting one element's
//! box outright stays on
//! [`DeckEdit::update_transform`](super::edit::DeckEdit::update_transform).
//!
//! Every operation mutates the draft in place; atomicity and rollback come from
//! [`Deck::transaction`](super::model::Deck::transaction). Object identities are
//! preserved: alignment and distribution rewrite only the transforms, and a
//! z-order move re-seats the very same element.

use super::{
    edit::DeckEdit,
    error::{DeckError, codes, params::ObjectKind},
    id::{ElementId, SlideId},
    model::Transform,
};

/// Edge an alignment run pins the selected boxes to; only its axis moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignEdge {
    /// Pin the left edges.
    Left,
    /// Center horizontally inside the reference frame.
    Center,
    /// Pin the right edges.
    Right,
    /// Pin the top edges.
    Top,
    /// Center vertically inside the reference frame.
    Middle,
    /// Pin the bottom edges.
    Bottom,
}

impl AlignEdge {
    /// Whether the edge belongs to the horizontal axis.
    pub fn horizontal(self) -> bool {
        matches!(self, Self::Left | Self::Center | Self::Right)
    }
}

/// Reference frame of an alignment run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlignReference {
    /// The whole slide area.
    #[default]
    Slide,
    /// The joint bounding box of the selected elements.
    Selection,
}

/// Axis along which elements are distributed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistributeAxis {
    /// Space boxes along the x axis.
    Horizontal,
    /// Space boxes along the y axis.
    Vertical,
}

impl DistributeAxis {
    fn start(self, transform: &Transform) -> i64 {
        match self {
            Self::Horizontal => transform.x_emu,
            Self::Vertical => transform.y_emu,
        }
    }

    fn set_start(self, transform: &mut Transform, value: i64) {
        match self {
            Self::Horizontal => transform.x_emu = value,
            Self::Vertical => transform.y_emu = value,
        }
    }

    fn size(self, transform: &Transform) -> i64 {
        match self {
            Self::Horizontal => transform.width_emu,
            Self::Vertical => transform.height_emu,
        }
    }
}

/// Paint-order move accepted by [`DeckEdit::set_element_z_order`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElementZOrder {
    /// Paint last, on top of everything.
    Front,
    /// Paint first, below everything.
    Back,
    /// One step towards the front.
    Forward,
    /// One step towards the back.
    Backward,
}

impl DeckEdit {
    /// Aligns the selected boxes to the slide or to their joint bounding box.
    ///
    /// Positions are computed from one snapshot, then written in one pass, so
    /// the order of the list never changes the result. The list must be
    /// non-empty, duplicate-free and every element must live on the slide.
    pub fn align_elements(
        &mut self,
        slide: &SlideId,
        elements: &[ElementId],
        edge: AlignEdge,
        reference: AlignReference,
    ) -> Result<(), DeckError> {
        assert_selection(elements, 1)?;
        let frames = self.element_frames(slide, elements)?;
        let bounds = match reference {
            AlignReference::Slide => {
                let size = self.document().size;
                Bounds { x: 0, y: 0, width: size.width_emu, height: size.height_emu }
            }
            AlignReference::Selection => selection_bounds(&frames),
        };
        let updates = elements
            .iter()
            .zip(frames)
            .map(|(id, transform)| {
                let mut next = transform;
                match edge {
                    AlignEdge::Left => next.x_emu = bounds.x,
                    AlignEdge::Center => next.x_emu = bounds.x + (bounds.width - next.width_emu) / 2,
                    AlignEdge::Right => next.x_emu = bounds.x + bounds.width - next.width_emu,
                    AlignEdge::Top => next.y_emu = bounds.y,
                    AlignEdge::Middle => next.y_emu = bounds.y + (bounds.height - next.height_emu) / 2,
                    AlignEdge::Bottom => next.y_emu = bounds.y + bounds.height - next.height_emu,
                }
                (id.clone(), next)
            })
            .collect::<Vec<_>>();
        for (id, transform) in updates {
            self.update_transform(slide, &id, transform)?;
        }
        Ok(())
    }

    /// Spaces the middle boxes evenly between the fixed first and last boxes.
    ///
    /// At least three elements are required; the outermost boxes along `axis`
    /// keep their positions. Positions are computed from one snapshot and the
    /// list order never matters.
    pub fn distribute_elements(&mut self, slide: &SlideId, elements: &[ElementId], axis: DistributeAxis) -> Result<(), DeckError> {
        assert_selection(elements, 3)?;
        let frames = self.element_frames(slide, elements)?;
        let mut ordered = elements
            .iter()
            .cloned()
            .zip(frames)
            .enumerate()
            .map(|(index, (id, transform))| (axis.start(&transform), index, id, transform))
            .collect::<Vec<_>>();
        ordered.sort_by_key(|(start, index, _, _)| (*start, *index));

        let start = ordered.first().map_or(0, |(start, _, _, _)| *start);
        let end = ordered.last().map_or(start, |(_, _, _, transform)| axis.start(transform).saturating_add(axis.size(transform)));
        let total = ordered.iter().map(|(_, _, _, transform)| axis.size(transform)).sum::<i64>();
        let gaps = (ordered.len() - 1) as i64;
        let spacing = end.saturating_sub(start).saturating_sub(total) / gaps;

        let last = ordered.len() - 1;
        let mut cursor = start;
        let mut updates = Vec::with_capacity(last.saturating_sub(1));
        for (position, (_, _, id, transform)) in ordered.iter().enumerate() {
            if position != 0 && position != last {
                let mut next = *transform;
                axis.set_start(&mut next, cursor);
                updates.push((id.clone(), next));
            }
            cursor = cursor.saturating_add(axis.size(transform)).saturating_add(spacing);
        }
        for (id, transform) in updates {
            self.update_transform(slide, &id, transform)?;
        }
        Ok(())
    }

    /// Re-seats one element in the slide's paint order, keeping its identity.
    ///
    /// The element is detached first, so `index` counts the remaining elements;
    /// an index beyond the end places the element last. Returns the final
    /// position.
    pub fn move_element_to(&mut self, slide: &SlideId, element: &ElementId, index: usize) -> Result<usize, DeckError> {
        let model = self.slide_mut(slide)?;
        let position = model.elements.iter().position(|candidate| candidate.id == *element).ok_or_else(|| missing_element(slide, element))?;
        let moved = model.elements.remove(position);
        let index = index.min(model.elements.len());
        model.elements.insert(index, moved);
        Ok(index)
    }

    /// Moves one element through the paint order, keeping its identity.
    ///
    /// Steps that would leave the stack stay put: bringing the topmost element
    /// forward is a no-op that still reports the current position.
    pub fn set_element_z_order(&mut self, slide: &SlideId, element: &ElementId, movement: ElementZOrder) -> Result<usize, DeckError> {
        let model = self.document().slide(slide).ok_or_else(|| missing_slide(slide))?;
        let position = model.elements.iter().position(|candidate| candidate.id == *element).ok_or_else(|| missing_element(slide, element))?;
        let last = model.elements.len().saturating_sub(1);
        let index = match movement {
            ElementZOrder::Front => last,
            ElementZOrder::Back => 0,
            ElementZOrder::Forward => (position + 1).min(last),
            ElementZOrder::Backward => position.saturating_sub(1),
        };
        self.move_element_to(slide, element, index)
    }

    /// Snapshots the frames of the selected elements, rejecting unknown ids.
    fn element_frames(&self, slide: &SlideId, elements: &[ElementId]) -> Result<Vec<Transform>, DeckError> {
        let model = self.document().slide(slide).ok_or_else(|| missing_slide(slide))?;
        elements.iter().map(|id| model.element(id).map(|element| element.transform).ok_or_else(|| missing_element(slide, id))).collect()
    }
}

/// The joint bounding box of one snapshot of frames.
#[derive(Debug, Clone, Copy)]
struct Bounds {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
}

fn selection_bounds(frames: &[Transform]) -> Bounds {
    let mut x = i64::MAX;
    let mut y = i64::MAX;
    let mut right = i64::MIN;
    let mut bottom = i64::MIN;
    for transform in frames {
        x = x.min(transform.x_emu);
        y = y.min(transform.y_emu);
        right = right.max(transform.x_emu.saturating_add(transform.width_emu));
        bottom = bottom.max(transform.y_emu.saturating_add(transform.height_emu));
    }
    Bounds { x, y, width: right.saturating_sub(x), height: bottom.saturating_sub(y) }
}

fn assert_selection(elements: &[ElementId], minimum: usize) -> Result<(), DeckError> {
    if elements.len() < minimum {
        return Err(invalid(format!("a layout selection needs at least {minimum} element ids")));
    }
    let mut seen = std::collections::BTreeSet::new();
    for id in elements {
        if !seen.insert(id.clone()) {
            return Err(invalid(format!("element `{id}` appears twice in the selection")));
        }
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> DeckError {
    DeckError::new(codes::ARGUMENT_INVALID, message)
}

fn missing_slide(slide: &SlideId) -> DeckError {
    DeckError::unknown(ObjectKind::Slide, slide.as_str()).with_slide(slide.clone())
}

fn missing_element(slide: &SlideId, element: &ElementId) -> DeckError {
    DeckError::unknown(ObjectKind::Element, element.as_str())
        .with_slide(slide.clone())
        .with_element(element.clone())
}
