//! Editable authored presentation model.
//!
//! `authored` is the authoring truth of a deck. `display` and `flow` stay
//! render projections: they are produced from a committed authored revision and
//! are never written back. Playback, rendering and the agent authoring surface
//! all consume the same revision, so no entry point grows a second deck model.
//!
//! The module owns identities, revisions, transactions, the native project
//! container, static playback and rule checks. PPTX exchange, chart drawing,
//! formula layout and media execution stay in their own upstream capability
//! trees and are only referenced from here.

pub mod edit;
pub mod error;
pub mod id;
pub mod inspect;
pub mod model;
pub mod playback;
pub mod project;
pub mod size;

pub use edit::{
    ChartElementInput, DeckEdit, DefaultBox, FormulaElementInput, ImageElementInput, ShapeElementInput, SlideAddOptions, TableElementInput,
    TextElementInput,
};
pub use error::{DeckError, codes};
pub use id::{AssetId, DeckId, ElementId, IdAllocator, LayoutId, MasterId, SlideId, ThemeId};
pub use inspect::{CHECKER_VERSION, InspectCheck, InspectIssue, InspectOptions, InspectReport, Severity};
pub use model::{
    Asset, ChartContent, ChartData, ChartSeries, ChartType, Crop, Deck, DeckCreateOptions, DeckDocument, DeckSlide, Element, ElementContent,
    ElementKind, ElementOrigin, ElementStyle, Fill, FormulaContent, HISTORY_LIMIT, ImageContent, Layout, Line, LockFlags, Master, MediaContent,
    MediaKind, OriginKind, PROJECT_FORMAT, PROJECT_VERSION, PlaceholderRef, ShapeContent, TableCell, TableContent, TableRow, TextAlign,
    TextContent, TextParagraph, TextRun, Theme, Transform,
};
pub use playback::{PlaybackSession, PlaybackState};
pub use project::{DECK_ENTRY, decode_project, encode_project};
pub use size::{DeckSize, EMU_PER_POINT, emu_to_points, points_to_emu};
