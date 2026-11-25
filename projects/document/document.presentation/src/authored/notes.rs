//! Speaker-notes operations on a deck.
//!
//! [`DeckEdit::set_slide_notes`](super::edit::DeckEdit::set_slide_notes) writes
//! the full text of one slide; the operations here append to and clear notes,
//! and [`Deck::notes_outline`] reads the whole deck's notes in playback order —
//! the shape a presenter view or a notes export needs.
//!
//! Notes stay plain text in the authored model. Rich formatting of notes belongs
//! to the product surface, not to the deck truth.

use serde::Serialize;

use super::{
    edit::DeckEdit,
    error::{DeckError, codes},
    id::SlideId,
    model::{Deck, DeckSlide},
};

/// One slide's notes entry as reported by [`Deck::notes_outline`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotesEntry {
    /// Slide identity.
    pub slide: SlideId,
    /// Zero-based playback position.
    pub index: usize,
    /// Plain text of the title placeholder, empty when the slide has none.
    pub title: String,
    /// Whether playback skips this slide.
    pub hidden: bool,
    /// Speaker notes, `None` when the slide carries none.
    pub notes: Option<String>,
}

impl Deck {
    /// Every slide's notes in playback order, hidden slides included and flagged.
    pub fn notes_outline(&self) -> Vec<NotesEntry> {
        self.slides()
            .iter()
            .enumerate()
            .map(|(index, slide)| NotesEntry {
                slide: slide.id.clone(),
                index,
                title: slide.title(),
                hidden: slide.hidden,
                notes: slide.notes.clone(),
            })
            .collect()
    }
}

impl DeckEdit {
    /// Appends a paragraph to one slide's notes.
    ///
    /// Existing notes are kept and the new text starts on its own line. The
    /// appended text must not be blank.
    pub fn append_slide_notes(&mut self, slide: &SlideId, text: impl Into<String>) -> Result<(), DeckError> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(DeckError::new(codes::ARGUMENT_INVALID, "appended notes must not be blank"));
        }
        let slide = self.slide_mut(slide)?;
        append_notes(slide, &text);
        Ok(())
    }

    /// Appends the same paragraph to every slide's notes, e.g. a global reminder.
    pub fn append_notes_to_all(&mut self, text: impl Into<String>) -> Result<usize, DeckError> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(DeckError::new(codes::ARGUMENT_INVALID, "appended notes must not be blank"));
        }
        let mut touched = 0usize;
        for slide in &mut self.document_mut().slides {
            append_notes(slide, &text);
            touched += 1;
        }
        Ok(touched)
    }

    /// Clears one slide's notes.
    pub fn clear_slide_notes(&mut self, slide: &SlideId) -> Result<(), DeckError> {
        self.slide_mut(slide)?.notes = None;
        Ok(())
    }
}

fn append_notes(slide: &mut DeckSlide, text: &str) {
    match &mut slide.notes {
        Some(existing) if !existing.is_empty() => {
            existing.push('\n');
            existing.push_str(text);
        }
        _ => slide.notes = Some(text.to_string()),
    }
}
