//! Static playback over one committed revision.
//!
//! A playback session pins the revision it started from. Editing the deck
//! afterwards does not silently change what is on screen; the caller must reload
//! or rebase explicitly. Build steps, transitions and media belong to a later
//! phase and are not simulated here.

use super::{
    error::{DeckError, codes, params::ObjectKind},
    id::SlideId,
    model::Deck,
};

/// Observable state of a playback session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackState {
    /// Revision the session is pinned to.
    pub revision: u64,
    /// Current slide identity.
    pub slide_id: Option<SlideId>,
    /// Build step inside the current slide; always `0` in the static phase.
    pub build_step: u32,
    /// Time spent inside the current slide in milliseconds.
    pub time_ms: u64,
    /// Whether the show is currently advancing.
    pub playing: bool,
}

/// A static playback session.
#[derive(Debug, Clone)]
pub struct PlaybackSession {
    revision: u64,
    slides: Vec<SlideId>,
    notes: Vec<Option<String>>,
    position: Option<usize>,
    playing: bool,
    build_step: u32,
    time_ms: u64,
}

impl PlaybackSession {
    /// Pinned revision.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Visible slides in show order; hidden slides are absent.
    pub fn slides(&self) -> &[SlideId] {
        &self.slides
    }

    /// Current observable state.
    pub fn state(&self) -> PlaybackState {
        PlaybackState {
            revision: self.revision,
            slide_id: self.position.and_then(|position| self.slides.get(position).cloned()),
            build_step: self.build_step,
            time_ms: self.time_ms,
            playing: self.playing,
        }
    }

    /// Speaker notes of the current slide.
    pub fn notes(&self) -> Option<&str> {
        self.position.and_then(|position| self.notes[position].as_deref())
    }

    /// Starts the show at the first visible slide.
    pub fn start(&mut self) -> Result<(), DeckError> {
        if self.slides.is_empty() {
            return Err(DeckError::new(codes::PLAYBACK_INVALID_STATE, "the deck has no visible slides"));
        }
        self.position = Some(0);
        self.playing = true;
        self.build_step = 0;
        self.time_ms = 0;
        Ok(())
    }

    /// Pauses an advancing show.
    pub fn pause(&mut self) -> Result<(), DeckError> {
        self.assert_running()?;
        self.playing = false;
        Ok(())
    }

    /// Resumes a paused show.
    pub fn resume(&mut self) -> Result<(), DeckError> {
        self.assert_started()?;
        if self.playing {
            return Err(DeckError::new(codes::PLAYBACK_INVALID_STATE, "the show is already running"));
        }
        self.playing = true;
        Ok(())
    }

    /// Advances one slide. On the last slide the show stops where it is.
    ///
    /// The name mirrors the authoring surface (`player.next()`), not `Iterator`.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<(), DeckError> {
        self.assert_started()?;
        let position = self.position.unwrap_or(0);
        if position + 1 >= self.slides.len() {
            self.playing = false;
            return Ok(());
        }
        self.position = Some(position + 1);
        self.build_step = 0;
        self.time_ms = 0;
        Ok(())
    }

    /// Returns one slide. On the first slide the show stays there.
    pub fn previous(&mut self) -> Result<(), DeckError> {
        self.assert_started()?;
        let position = self.position.unwrap_or(0);
        self.position = Some(position.saturating_sub(1));
        self.build_step = 0;
        self.time_ms = 0;
        Ok(())
    }

    /// Jumps to a visible slide.
    pub fn go_to(&mut self, slide_id: &SlideId) -> Result<(), DeckError> {
        self.assert_started()?;
        let position = self.slides.iter().position(|candidate| candidate == slide_id).ok_or_else(|| {
            DeckError::new(
                codes::OBJECT_NOT_FOUND,
                format!("slide `{slide_id}` is not part of this show; hidden slides are skipped"),
            )
            .with_object(ObjectKind::Slide, slide_id.as_str())
        })?;
        self.position = Some(position);
        self.build_step = 0;
        self.time_ms = 0;
        Ok(())
    }

    /// Stops and rewinds to the first slide.
    pub fn stop(&mut self) {
        self.position = Some(0);
        self.playing = false;
        self.build_step = 0;
        self.time_ms = 0;
    }

    fn assert_started(&self) -> Result<(), DeckError> {
        if self.position.is_none() {
            return Err(DeckError::new(codes::PLAYBACK_INVALID_STATE, "the show has not started; call start() first").with_recoverable(true));
        }
        Ok(())
    }

    fn assert_running(&self) -> Result<(), DeckError> {
        self.assert_started()?;
        if !self.playing {
            return Err(DeckError::new(codes::PLAYBACK_INVALID_STATE, "the show is already paused").with_recoverable(true));
        }
        Ok(())
    }
}

impl Deck {
    /// Creates a playback session pinned to the current revision.
    ///
    /// `start_at` selects the first slide; it must be a visible slide identity.
    pub fn playback(&self, start_at: Option<&SlideId>) -> Result<PlaybackSession, DeckError> {
        let mut slides = Vec::new();
        let mut notes = Vec::new();
        for slide in self.slides().iter().filter(|slide| !slide.hidden) {
            slides.push(slide.id.clone());
            notes.push(slide.notes.clone());
        }
        let position = match start_at {
            Some(id) => Some(slides.iter().position(|candidate| candidate == id).ok_or_else(|| {
                DeckError::new(
                    codes::OBJECT_NOT_FOUND,
                    format!("slide `{id}` is not part of this show; hidden slides are skipped"),
                )
                .with_object(ObjectKind::Slide, id.as_str())
            })?),
            None => None,
        };
        Ok(PlaybackSession { revision: self.revision(), slides, notes, position, playing: false, build_step: 0, time_ms: 0 })
    }
}
