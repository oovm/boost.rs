//! Typed shape-style operations on a [`DeckEdit`] draft.
//!
//! These methods carry the fill and stroke edits the OpenPPT product exposed
//! through JSON script operations (`element.setFill`, `element.setLine`), but
//! with typed Rust inputs. Shape creation stays on
//! [`DeckEdit::add_shape`](super::edit::DeckEdit::add_shape); table-cell fills
//! stay on [`DeckEdit::set_table_cell_fill`](super::tables::DeckEdit::set_table_cell_fill).
//!
//! Every operation mutates the draft in place; atomicity and rollback come from
//! [`Deck::transaction`](super::model::Deck::transaction). The element identity
//! is preserved, and a non-shape target is rejected before anything changes.

use super::{
    edit::DeckEdit,
    error::{DeckError, codes},
    id::{ElementId, SlideId},
    model::{ElementContent, Fill, Line, ShapeContent},
};

impl DeckEdit {
    /// Sets or clears a shape's fill override.
    pub fn set_shape_fill(&mut self, slide: &SlideId, element: &ElementId, fill: Option<Fill>) -> Result<(), DeckError> {
        self.shape_mut(slide, element, "only shapes carry a fill")?.fill = fill;
        Ok(())
    }

    /// Sets or clears a shape's stroke override.
    pub fn set_shape_line(&mut self, slide: &SlideId, element: &ElementId, line: Option<Line>) -> Result<(), DeckError> {
        self.shape_mut(slide, element, "only shapes carry a line")?.line = line;
        Ok(())
    }

    /// Borrows the shape content of one element, rejecting non-shape targets.
    fn shape_mut(&mut self, slide: &SlideId, element: &ElementId, needs: &str) -> Result<&mut ShapeContent, DeckError> {
        let model = self.slide_mut(slide)?;
        let target = model.element_mut(element).ok_or_else(|| missing_element(slide, element))?;
        match &mut target.content {
            ElementContent::Shape(shape) => Ok(shape),
            _ => Err(DeckError::new(codes::OBJECT_TYPE_MISMATCH, format!("element `{element}` is not a shape; {needs}"))
                .with_slide(slide.clone())
                .with_element(element.clone())),
        }
    }
}

fn missing_element(slide: &SlideId, element: &ElementId) -> DeckError {
    DeckError::new(codes::OBJECT_NOT_FOUND, format!("element `{element}` does not exist on this slide"))
        .with_slide(slide.clone())
        .with_element(element.clone())
}
