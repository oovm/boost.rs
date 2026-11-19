//! Stable error surface of the authored deck model.
//!
//! Codes are contract: the TypeScript authoring surface, the CLI and the skill
//! documents compare them, so the message may be reworded while the code stays.

use super::id::{ElementId, SlideId};

/// Stable codes reported by authored deck operations.
pub mod codes {
    /// The deck handle is gone or was never created.
    pub const DECK_NOT_FOUND: &str = "deck/not-found";
    /// `expected_revision` did not match the committed revision.
    pub const DECK_REVISION_CONFLICT: &str = "deck/revision-conflict";
    /// A slide, element, layout, master, theme or asset identity is unknown.
    pub const OBJECT_NOT_FOUND: &str = "object/not-found";
    /// A patch does not match the addressed element type.
    pub const OBJECT_TYPE_MISMATCH: &str = "object/type-mismatch";
    /// The object type exists in the format but is not editable yet.
    pub const OBJECT_UNSUPPORTED: &str = "object/unsupported";
    /// A referenced asset has no bytes or record.
    pub const ASSET_MISSING: &str = "asset/missing";
    /// An argument is unusable, such as an empty identity or a negative size.
    pub const ARGUMENT_INVALID: &str = "session/argument-invalid";
    /// A native project container is malformed or fails its own checks.
    pub const FORMAT_INVALID_PACKAGE: &str = "format/invalid-package";
    /// A format or container version is not supported by this build.
    pub const FORMAT_UNSUPPORTED: &str = "format/unsupported";
    /// The playback session was asked to do something illegal in its state.
    pub const PLAYBACK_INVALID_STATE: &str = "playback/invalid-state";
}

/// One authored-model failure with its locators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeckError {
    code: &'static str,
    message: String,
    revision: Option<u64>,
    slide_id: Option<SlideId>,
    element_id: Option<ElementId>,
    recoverable: bool,
}

impl DeckError {
    /// Creates an error from a stable code and a message.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), revision: None, slide_id: None, element_id: None, recoverable: false }
    }

    /// Locates the failure at a committed revision.
    pub fn with_revision(mut self, revision: u64) -> Self {
        self.revision = Some(revision);
        self
    }

    /// Locates the failure at a slide.
    pub fn with_slide(mut self, slide_id: SlideId) -> Self {
        self.slide_id = Some(slide_id);
        self
    }

    /// Locates the failure at an element.
    pub fn with_element(mut self, element_id: ElementId) -> Self {
        self.element_id = Some(element_id);
        self
    }

    /// Marks whether retrying with different input or state can succeed.
    pub fn with_recoverable(mut self, recoverable: bool) -> Self {
        self.recoverable = recoverable;
        self
    }

    /// Stable, comparable code.
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// Human-readable message; never carries binary payloads.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Revision the failure belongs to, when known.
    pub fn revision(&self) -> Option<u64> {
        self.revision
    }

    /// Slide the failure belongs to, when known.
    pub fn slide_id(&self) -> Option<&SlideId> {
        self.slide_id.as_ref()
    }

    /// Element the failure belongs to, when known.
    pub fn element_id(&self) -> Option<&ElementId> {
        self.element_id.as_ref()
    }

    /// Whether the caller can retry successfully.
    pub fn recoverable(&self) -> bool {
        self.recoverable
    }
}

impl std::fmt::Display for DeckError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DeckError {}
