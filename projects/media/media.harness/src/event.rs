//! Recorded events and their classification.

/// Monotonic event sequence number inside one [`Trace`](crate::Trace).
pub type Seq = u64;

/// How serious an event is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Severity {
    /// Verbose diagnostic detail.
    Trace,
    /// Normal progress.
    Info,
    /// Something degraded but kept running.
    Warning,
    /// An operation failed.
    Error,
}

impl Severity {
    /// Uppercase label used in reports.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Info => "INFO",
            Self::Warning => "WARN",
            Self::Error => "ERROR",
        }
    }
}

/// Execution phase an event belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Stage {
    /// Reading a project, media asset or font.
    Load,
    /// Project, scene and resource-contract checks.
    Validate,
    /// Sound-effect or voice synthesis.
    Synthesize,
    /// Audio mixing and level control.
    Mix,
    /// Frame or item rasterization.
    Paint,
    /// Video/audio encoding.
    Encode,
    /// Atomic output rename and stats.
    Publish,
    /// Anything that does not fit the other stages.
    Other,
}

/// What an asset is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AssetKind {
    /// Still image.
    Image,
    /// Video clip or sequence.
    Video,
    /// Audio clip or narration WAV.
    Audio,
    /// Font face.
    Font,
    /// Anything else.
    Other,
}

impl AssetKind {
    /// Lowercase label used in reports.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Font => "font",
            Self::Other => "asset",
        }
    }
}

/// One recorded fact.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum EventKind {
    /// A project was parsed into concrete dimensions, frames and scenes.
    ProjectLoaded {
        /// Scene count.
        scenes: usize,
        /// Total project frames.
        frames: u32,
        /// Project frame rate.
        fps: u32,
        /// Output width in pixels.
        width: u32,
        /// Output height in pixels.
        height: u32,
    },
    /// A declared asset was decoded.
    AssetLoaded {
        /// Asset path as declared.
        path: String,
        /// Asset role.
        kind: AssetKind,
        /// Decoded byte length, when known.
        bytes: u64,
    },
    /// A declared asset was absent.
    AssetMissing {
        /// Asset path as declared.
        path: String,
        /// Asset role.
        kind: AssetKind,
        /// Where the asset was referenced, e.g. `scene 2 item 1`.
        slot: String,
        /// Whether the contract required it.
        required: bool,
    },
    /// A declared asset existed but failed to decode.
    AssetDecodeFailed {
        /// Asset path as declared.
        path: String,
        /// Asset role.
        kind: AssetKind,
        /// Failure message.
        message: String,
    },
    /// A font face was resolved to a real file.
    FontResolved {
        /// Role name, e.g. `regular`.
        role: String,
        /// File path actually loaded.
        path: String,
    },
    /// A font face could not be resolved or decoded.
    FontMissing {
        /// Role name.
        role: String,
        /// Failure message.
        message: String,
    },
    /// A project or scene contract check rejected its input.
    ValidationFailed {
        /// What was validated.
        subject: String,
        /// Failure message.
        message: String,
    },
    /// A sound effect or voice track was synthesized.
    Synthesized {
        /// Subject name, e.g. `impact-deep` or `voice`.
        subject: String,
        /// Rendered duration in seconds.
        seconds: f64,
        /// Peak level in dBFS.
        peak_db: f32,
        /// RMS level in dBFS.
        rms_db: f32,
        /// Whether any sample reached full scale.
        clipped: bool,
    },
    /// Synthesis failed for one subject.
    SynthesisFailed {
        /// Subject name.
        subject: String,
        /// Failure message.
        message: String,
    },
    /// The audio bed was mixed to the final interleaved buffer.
    Mixed {
        /// Mixed duration in seconds.
        seconds: f64,
        /// Peak level in dBFS.
        peak_db: f32,
        /// RMS level in dBFS.
        rms_db: f32,
        /// Whether any sample reached full scale.
        clipped: bool,
    },
    /// Mixing failed.
    MixFailed {
        /// Failure message.
        message: String,
    },
    /// One scene finished painting.
    ScenePainted {
        /// Scene index.
        scene: usize,
        /// Frames painted for this scene.
        frames: u32,
    },
    /// A requested frame was never painted.
    FrameMissed {
        /// Scene index.
        scene: usize,
        /// Project frame number.
        frame: u32,
    },
    /// Painting a scene or item failed.
    PaintFailed {
        /// Scene index.
        scene: usize,
        /// Item index inside the scene, when the failure is item-local.
        item: Option<usize>,
        /// Failure message.
        message: String,
    },
    /// The encoder accepted a configuration.
    EncodeOpened {
        /// Encoder description, e.g. `media-foundation+h264+aac`.
        backend: String,
        /// Output width.
        width: u32,
        /// Output height.
        height: u32,
        /// Output frame rate.
        fps: u32,
        /// Frames the run intends to write.
        frames: u32,
    },
    /// Encoding finished and the file was closed.
    Encoded {
        /// Frames written.
        frames: u32,
        /// Encoded byte length.
        bytes: u64,
    },
    /// Encoding failed.
    EncodeFailed {
        /// Failure message.
        message: String,
    },
    /// The run stopped on an explicit cancellation.
    Cancelled {
        /// Frame the cancellation was observed at.
        frame: u32,
    },
    /// The artifact was published under its final name.
    Published {
        /// Final output path.
        path: String,
        /// Final byte length.
        bytes: u64,
    },
    /// Publishing the artifact failed.
    PublishFailed {
        /// Final output path.
        path: String,
        /// Failure message.
        message: String,
    },
    /// Execution fell back to a less capable path.
    Degraded {
        /// Path that was requested.
        from: String,
        /// Path that actually ran.
        to: String,
        /// Why the fallback happened.
        reason: String,
    },
    /// Free-form diagnostic note.
    Note {
        /// Note text.
        message: String,
    },
}

impl EventKind {
    /// Severity implied by the event kind.
    pub const fn severity(&self) -> Severity {
        match self {
            Self::AssetMissing { required: true, .. }
            | Self::AssetDecodeFailed { .. }
            | Self::FontMissing { .. }
            | Self::ValidationFailed { .. }
            | Self::SynthesisFailed { .. }
            | Self::MixFailed { .. }
            | Self::PaintFailed { .. }
            | Self::EncodeFailed { .. }
            | Self::PublishFailed { .. } => Severity::Error,
            Self::AssetMissing { required: false, .. } | Self::FrameMissed { .. } | Self::Cancelled { .. } | Self::Degraded { .. } => {
                Severity::Warning
            }
            Self::ProjectLoaded { .. }
            | Self::AssetLoaded { .. }
            | Self::FontResolved { .. }
            | Self::Synthesized { .. }
            | Self::Mixed { .. }
            | Self::ScenePainted { .. }
            | Self::EncodeOpened { .. }
            | Self::Encoded { .. }
            | Self::Published { .. } => Severity::Info,
            Self::Note { .. } => Severity::Trace,
        }
    }

    /// Execution phase implied by the event kind.
    pub const fn stage(&self) -> Stage {
        match self {
            Self::ProjectLoaded { .. } | Self::AssetLoaded { .. } | Self::AssetMissing { .. } | Self::AssetDecodeFailed { .. } => Stage::Load,
            Self::FontResolved { .. } | Self::FontMissing { .. } | Self::ValidationFailed { .. } => Stage::Validate,
            Self::Synthesized { .. } | Self::SynthesisFailed { .. } => Stage::Synthesize,
            Self::Mixed { .. } | Self::MixFailed { .. } => Stage::Mix,
            Self::ScenePainted { .. } | Self::FrameMissed { .. } | Self::PaintFailed { .. } => Stage::Paint,
            Self::EncodeOpened { .. } | Self::Encoded { .. } | Self::EncodeFailed { .. } => Stage::Encode,
            Self::Published { .. } | Self::PublishFailed { .. } => Stage::Publish,
            Self::Cancelled { .. } | Self::Degraded { .. } | Self::Note { .. } => Stage::Other,
        }
    }

    /// Short human-readable summary for timelines.
    pub fn summary(&self) -> String {
        match self {
            Self::ProjectLoaded { scenes, frames, fps, width, height } => {
                format!("project {width}x{height} {fps} fps, {frames} frames, {scenes} scenes")
            }
            Self::AssetLoaded { path, kind, bytes } => format!("load {} `{path}` ({bytes} bytes)", kind.label()),
            Self::AssetMissing { path, kind, slot, required } => {
                format!("{} `{path}` missing at {slot} (required={required})", kind.label())
            }
            Self::AssetDecodeFailed { path, kind, message } => format!("{} `{path}` decode failed: {message}", kind.label()),
            Self::FontResolved { role, path } => format!("font `{role}` -> {path}"),
            Self::FontMissing { role, message } => format!("font `{role}` unavailable: {message}"),
            Self::ValidationFailed { subject, message } => format!("validation `{subject}` failed: {message}"),
            Self::Synthesized { subject, seconds, peak_db, rms_db, clipped } => {
                format!("synthesize `{subject}` {seconds:.2}s peak {peak_db:.1} dBFS rms {rms_db:.1} dBFS clipped={clipped}")
            }
            Self::SynthesisFailed { subject, message } => format!("synthesize `{subject}` failed: {message}"),
            Self::Mixed { seconds, peak_db, rms_db, clipped } => {
                format!("mix {seconds:.2}s peak {peak_db:.1} dBFS rms {rms_db:.1} dBFS clipped={clipped}")
            }
            Self::MixFailed { message } => format!("mix failed: {message}"),
            Self::ScenePainted { scene, frames } => format!("scene {scene} painted {frames} frames"),
            Self::FrameMissed { scene, frame } => format!("scene {scene} missed frame {frame}"),
            Self::PaintFailed { scene, item, message } => match item {
                Some(item) => format!("paint scene {scene} item {item} failed: {message}"),
                None => format!("paint scene {scene} failed: {message}"),
            },
            Self::EncodeOpened { backend, width, height, fps, frames } => {
                format!("encode `{backend}` {width}x{height} {fps} fps, {frames} frames")
            }
            Self::Encoded { frames, bytes } => format!("encoded {frames} frames ({bytes} bytes)"),
            Self::EncodeFailed { message } => format!("encode failed: {message}"),
            Self::Cancelled { frame } => format!("cancelled at frame {frame}"),
            Self::Published { path, bytes } => format!("published {path} ({bytes} bytes)"),
            Self::PublishFailed { path, message } => format!("publish {path} failed: {message}"),
            Self::Degraded { from, to, reason } => format!("degraded `{from}` -> `{to}`: {reason}"),
            Self::Note { message } => message.clone(),
        }
    }
}

/// One recorded event with its sequence number.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Event {
    /// Monotonic sequence number.
    pub seq: Seq,
    /// Execution phase.
    pub stage: Stage,
    /// Event severity.
    pub severity: Severity,
    /// Recorded fact.
    pub kind: EventKind,
}
