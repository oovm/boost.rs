//! Scene IR: the whole film is one serializable data structure.
//!
//! The TypeScript authoring surface compiles to this shape and the renderer
//! consumes nothing else. Frame counts are the unit of time inside the IR;
//! seconds belong to the authoring layer.

use media_image::Fit;
use media_types::TimeBase;
use serde::{Deserialize, Serialize};

// Camera keyframes, easing and sampling live in the geometry library: a camera
// is a geometry pose plus a curve, and both are shared with downstream hosts.
pub use geometry::{CameraKey, CameraPose, Ease, camera_at};
// Animation evaluation (identity, tracks, time windows, path following) is the
// neutral `media.motion` core; the scene IR only carries the authored shape.
pub use media_motion::{Animated, Follow, Key, LoopMode, NodeId, TimeWindow, Track};

/// A complete renderable project.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Project {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub duration_frames: u32,
    pub scenes: Vec<Scene>,
    /// Reusable element definitions. Instances reference these by id, so one
    /// revision updates every placement instead of every copy.
    #[serde(default)]
    pub components: Vec<Component>,
    #[serde(default)]
    pub audio: Option<AudioSpec>,
    /// One camera curve for the whole film. When present it drives every frame
    /// continuously, which is what makes a one-take move possible.
    #[serde(default)]
    pub camera: Vec<CameraKey>,
    /// Visual output controls the author declares explicitly. Every field is
    /// opt-in: an omitted effect means "off", never a hidden house default.
    #[serde(default)]
    pub post: PostSpec,
    /// Explicit font files. When absent the host discovers fonts itself and
    /// must report the exact paths it resolved.
    #[serde(default)]
    pub fonts: Option<FontsSpec>,
    /// Semantic color card. Color fields may then reference a role as
    /// `$role`; [`crate::design::resolve_design`] rewrites them to concrete
    /// values and reports which scene and node used them.
    #[serde(default)]
    pub design: Option<crate::design::DesignSpec>,
    /// Extra raster resolution before the frame is downsampled, `1.0..=2.0`.
    /// Camera moves already force a supersampled world buffer; this lets a film
    /// ask for the same treatment when it has no camera, which is what keeps
    /// type and thin strokes smooth instead of stepped.
    #[serde(default)]
    pub supersample: Option<f32>,
}

/// Film-level visual output controls.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct PostSpec {
    /// Corner darkening strength, `0.0..=1.0`. `None` draws none.
    #[serde(default)]
    pub vignette: Option<f32>,
}

/// Explicit font file paths, one per face.
///
/// Declaring this removes system-font discovery from the render: a missing or
/// undecodable file is an error, not a silent fallback to another face.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct FontsSpec {
    pub regular: String,
    pub bold: String,
    pub mono: String,
}

impl FontsSpec {
    /// The three faces as `(role, path)` pairs, in declaration order.
    #[must_use]
    pub fn faces(&self) -> [(&'static str, &str); 3] {
        [("regular", self.regular.as_str()), ("bold", self.bold.as_str()), ("mono", self.mono.as_str())]
    }
}

impl Project {
    /// The project's rational time base, derived from the declared fps.
    /// Seconds and sample offsets must be computed through this single source
    /// instead of re-dividing the bare fields.
    #[must_use]
    pub fn time_base(&self) -> TimeBase {
        TimeBase::from_fps(self.fps)
    }
}

/// Which space a node is painted in.
///
/// A `World` node belongs to the drawing: it is painted into the scene buffer
/// and then moved by the lens. A `Screen` node skips that transform and is
/// painted onto the output frame after the camera, at the same scene-local
/// time, so an annotation keeps its size and screen position while the camera
/// pushes in. Screen space applies to top-level scene nodes; nodes inside a
/// component still paint into the component's own buffer.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Space {
    #[default]
    World,
    Screen,
}

impl Space {
    fn is_world(&self) -> bool {
        *self == Space::World
    }
}

/// One node: an item plus the identity and motion authored on it.
///
/// The item is flattened, so wire JSON keeps the historical shape
/// (`{ "kind": "text", … }`) and only gains optional `id`, `anim` and `space`
/// keys. That keeps existing films valid while giving every node a stable name
/// a revision or a lock can point at.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Node {
    /// Stable identity. Optional for legacy wire data, required by anything
    /// that references the node (instances, locks, review notes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<NodeId>,
    /// Motion declared on this node, in the node's local frames.
    #[serde(default, skip_serializing_if = "Animated::is_empty")]
    pub anim: Animated,
    /// Space the node is painted in; defaults to the camera's world.
    #[serde(default, skip_serializing_if = "Space::is_world")]
    pub space: Space,
    #[serde(flatten)]
    pub item: Item,
}

impl Node {
    /// Wraps an item as a node with no identity or motion.
    pub fn plain(item: Item) -> Self {
        Self { id: None, anim: Animated::default(), space: Space::World, item }
    }
}

/// A reusable element: a named list of nodes that instances place by id.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Component {
    pub id: NodeId,
    /// Nodes in paint order, timed in the component's own local frames.
    pub items: Vec<Node>,
}

/// One temporal block with a background and its items.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Scene {
    /// Stable identity for storyboard mapping, revision notes and locks.
    /// Optional for legacy wire data, but unique and non-empty when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub start: u32,
    pub duration: u32,
    #[serde(default)]
    pub background: Background,
    #[serde(default)]
    pub items: Vec<Node>,
    /// Camera move for this scene: content renders off-screen, then this
    /// transform pushes/pulls and pans it into the output frame.
    #[serde(default)]
    pub camera: Option<Camera>,
    /// Scene-level design overrides. A project entry marked `locked` cannot be
    /// overridden here; a different value is an explicit `invalid_scene`.
    #[serde(default)]
    pub design: Option<crate::design::DesignSpec>,
}

/// A single camera move, interpolated across the whole scene.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Camera {
    #[serde(default = "default_one")]
    pub zoom_from: f32,
    #[serde(default = "default_one")]
    pub zoom_to: f32,
    /// Focus point at the start and end, in normalized frame coordinates.
    #[serde(default = "default_center")]
    pub focus_from: [f32; 2],
    #[serde(default = "default_center")]
    pub focus_to: [f32; 2],
    #[serde(default)]
    pub ease: Ease,
    /// Extra rotation in degrees at the end of the move.
    #[serde(default)]
    pub rotate_to: f32,
}

fn default_center() -> [f32; 2] {
    [0.5, 0.5]
}

/// Painted behind the items for the life of the scene.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Background {
    Solid {
        color: String,
    },
    VGradient {
        colors: Vec<String>,
    },
    Radial {
        center: String,
        edge: String,
    },
    Starfield {
        base: Vec<String>,
        #[serde(default = "default_stars")]
        stars: u32,
        #[serde(default)]
        tint: Option<String>,
        /// Horizontal drift in pixels per second.
        #[serde(default)]
        drift: f32,
        /// Twinkle depth 0..=1.
        #[serde(default = "default_twinkle")]
        twinkle: f32,
    },
}

impl Default for Background {
    /// A scene that declares no background paints opaque black. The previous
    /// starfield default made every film inherit one house look.
    fn default() -> Self {
        Self::Solid { color: "#000000".to_string() }
    }
}

fn default_stars() -> u32 {
    140
}

fn default_twinkle() -> f32 {
    0.6
}

fn default_fade() -> u32 {
    10
}

fn default_stagger() -> u32 {
    1
}

fn default_one_f32() -> f32 {
    1.0
}

fn default_true() -> bool {
    true
}

/// Horizontal anchor of a text-anchored item.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Align {
    Left,
    #[default]
    Center,
    Right,
}

/// How a shape is composited onto the frame.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Blend {
    #[default]
    Normal,
    Add,
}

fn default_band_count() -> u32 {
    3
}
fn default_band_angle() -> f32 {
    -18.0
}
fn default_band_speed() -> f32 {
    26.0
}
fn default_band_alpha() -> f32 {
    0.08
}
fn default_band_thickness() -> f32 {
    90.0
}
fn default_grain() -> f32 {
    0.05
}
fn default_grain_density() -> u32 {
    9_000
}

fn default_layer_edge() -> f32 {
    0.25
}

/// Primitive shape kinds.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ShapeKind {
    Rect,
    Circle,
    Ring,
    Line,
}

/// Syntax tone of a code line fragment.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Tone {
    Plain,
    Keyword,
    String,
    Comment,
    Number,
    Fn,
}

/// Color roles for the built-in code highlighter.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CodeTheme {
    #[serde(default)]
    pub panel: Option<String>,
    #[serde(default = "default_plain")]
    pub plain: String,
    #[serde(default = "default_keyword")]
    pub keyword: String,
    #[serde(default = "default_string")]
    pub string: String,
    #[serde(default = "default_comment")]
    pub comment: String,
    #[serde(default = "default_number")]
    pub number: String,
    #[serde(default = "default_fn")]
    pub function: String,
}

impl Default for CodeTheme {
    fn default() -> Self {
        Self {
            panel: Some("#0d1526e0".to_string()),
            plain: default_plain(),
            keyword: default_keyword(),
            string: default_string(),
            comment: default_comment(),
            number: default_number(),
            function: default_fn(),
        }
    }
}

fn default_plain() -> String {
    "#d6e2f5".to_string()
}
fn default_keyword() -> String {
    "#7aa2ff".to_string()
}
fn default_string() -> String {
    "#6fd6a4".to_string()
}
fn default_comment() -> String {
    "#5d6f8c".to_string()
}
fn default_number() -> String {
    "#e8b86d".to_string()
}
fn default_fn() -> String {
    "#c792ea".to_string()
}

/// One animated element inside a scene. `at` and `dur` are scene-relative frames.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Item {
    Text {
        at: u32,
        dur: u32,
        content: String,
        #[serde(default)]
        sub: Option<String>,
        size: f32,
        #[serde(default)]
        bold: bool,
        color: String,
        #[serde(default)]
        sub_color: Option<String>,
        #[serde(default)]
        sub_size: Option<f32>,
        x: f32,
        y: f32,
        #[serde(default)]
        align: Align,
        #[serde(default = "default_fade")]
        fade_in: u32,
        #[serde(default = "default_fade")]
        fade_out: u32,
        #[serde(default)]
        drift_y: f32,
        #[serde(default)]
        max_width: Option<f32>,
        #[serde(default)]
        line_height: Option<f32>,
        #[serde(default)]
        typing: Option<f32>,
        #[serde(default)]
        glow: Option<String>,
        /// Character entrance: each glyph rises and fades in on a stagger.
        #[serde(default = "default_stagger")]
        stagger: u32,
        /// Pixels each glyph rises from, when `stagger` > 0.
        #[serde(default)]
        rise: f32,
        /// Reveal rectangle in normalized coordinates: glyphs only paint inside
        /// it. Used for text that grows out of a shape.
        #[serde(default)]
        clip: Option<[f32; 4]>,
    },
    Code {
        at: u32,
        dur: u32,
        lines: Vec<String>,
        x: f32,
        y: f32,
        size: f32,
        #[serde(default)]
        theme: CodeTheme,
        #[serde(default = "default_fade")]
        fade_in: u32,
        #[serde(default = "default_fade")]
        fade_out: u32,
        #[serde(default)]
        typing: Option<f32>,
        #[serde(default)]
        line_height: Option<f32>,
    },
    Counter {
        at: u32,
        dur: u32,
        from: f64,
        to: f64,
        x: f32,
        y: f32,
        size: f32,
        color: String,
        #[serde(default)]
        prefix: Option<String>,
        #[serde(default)]
        suffix: Option<String>,
        #[serde(default)]
        decimals: u32,
        #[serde(default)]
        ease: Ease,
        #[serde(default = "default_fade")]
        fade_in: u32,
        #[serde(default = "default_fade")]
        fade_out: u32,
    },
    Card {
        at: u32,
        dur: u32,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        title: String,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        accent: Option<String>,
        #[serde(default)]
        panel: Option<String>,
        #[serde(default = "default_fade")]
        fade_in: u32,
        #[serde(default = "default_fade")]
        fade_out: u32,
        #[serde(default)]
        drift_y: f32,
    },
    Shape {
        at: u32,
        dur: u32,
        shape: ShapeKind,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: String,
        #[serde(default)]
        radius: f32,
        #[serde(default)]
        stroke: Option<f32>,
        /// Fraction of the shape drawn, used for lines that draw themselves.
        #[serde(default = "default_one_f32")]
        progress: f32,
        /// `normal` or `add` (light-like overlays).
        #[serde(default)]
        blend: Blend,
        #[serde(default = "default_fade")]
        fade_in: u32,
        #[serde(default = "default_fade")]
        fade_out: u32,
    },
    /// Soft diagonal light bands that drift across the frame.
    Bands {
        at: u32,
        dur: u32,
        #[serde(default = "default_band_count")]
        count: u32,
        #[serde(default = "default_band_angle")]
        angle: f32,
        /// Vertical drift in pixels per second.
        #[serde(default = "default_band_speed")]
        speed: f32,
        color: String,
        #[serde(default = "default_band_alpha")]
        alpha: f32,
        #[serde(default = "default_band_thickness")]
        thickness: f32,
        #[serde(default = "default_fade")]
        fade_in: u32,
        #[serde(default = "default_fade")]
        fade_out: u32,
    },
    /// Deterministic per-frame film grain.
    Grain {
        at: u32,
        dur: u32,
        #[serde(default = "default_grain")]
        strength: f32,
        #[serde(default = "default_grain_density")]
        density: u32,
        #[serde(default = "default_fade")]
        fade_in: u32,
        #[serde(default = "default_fade")]
        fade_out: u32,
    },
    /// A filled and/or stroked path. Points are normalized frame coordinates.
    Path {
        at: u32,
        dur: u32,
        points: Vec<[f32; 2]>,
        /// Treat the points as a smooth Catmull-Rom curve instead of a polyline.
        #[serde(default)]
        smooth: bool,
        #[serde(default)]
        closed: bool,
        #[serde(default)]
        fill: Option<String>,
        /// Stroke color.
        color: String,
        #[serde(default)]
        stroke: Option<f32>,
        /// Dash length in design pixels for a dashed stroke; `None` is solid.
        ///
        /// Dashes are measured along the path length by the rasteriser, so a
        /// dashed technical line keeps its rhythm through curves instead of
        /// being approximated by separate segments.
        #[serde(default)]
        dash: Option<f32>,
        /// Gap between dashes in design pixels; defaults to the dash length.
        #[serde(default)]
        gap: Option<f32>,
        /// Fraction of the outline drawn, for a path that draws itself.
        #[serde(default = "default_one_f32")]
        progress: f32,
        /// Rotation in degrees around the path centroid.
        #[serde(default)]
        rotate: f32,
        /// Scale around the path centroid.
        #[serde(default = "default_one_f32")]
        scale: f32,
        #[serde(default)]
        blend: Blend,
        #[serde(default = "default_fade")]
        fade_in: u32,
        #[serde(default = "default_fade")]
        fade_out: u32,
    },
    /// A nested layer whose opacity follows the camera zoom. Layers crossing at
    /// the same zoom fade into each other, so a one-take push never cuts.
    Layer {
        at: u32,
        dur: u32,
        /// Zoom window in which this layer is fully visible.
        zoom_from: f32,
        zoom_to: f32,
        /// Width of the fade at both ends of the window, in zoom ratio.
        #[serde(default = "default_layer_edge")]
        edge: f32,
        #[serde(default)]
        items: Vec<Node>,
    },
    /// One placement of a reusable component, drawn into a layout box.
    ///
    /// The component is painted on its own and then placed like an image, so an
    /// instance is a real reuse boundary instead of a copy of the definition.
    Instance {
        at: u32,
        dur: u32,
        /// Id of an entry in [`Project::components`].
        component: NodeId,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        #[serde(default)]
        fit: Fit,
        #[serde(default = "default_one")]
        opacity: f32,
        #[serde(default = "default_fade")]
        fade_in: u32,
        #[serde(default = "default_fade")]
        fade_out: u32,
        /// See [`Item::Image`]: required sources must decode, and a missing
        /// component is always an error.
        #[serde(default = "default_required")]
        required: bool,
    },
    /// A soft additive light source. Additive falloff reads as light in air,
    /// which a flat disc never does.
    Glow {
        at: u32,
        dur: u32,
        x: f32,
        y: f32,
        /// Radius as a fraction of the frame width.
        radius: f32,
        #[serde(default = "default_glow_color")]
        color: String,
        /// Peak opacity at the center, 0..1.
        #[serde(default = "default_one")]
        intensity: f32,
        #[serde(default = "default_fade")]
        fade_in: u32,
        #[serde(default = "default_fade")]
        fade_out: u32,
    },
    Footnote {
        at: u32,
        dur: u32,
        text: String,
        #[serde(default = "default_note_size")]
        size: f32,
        x: f32,
        y: f32,
        #[serde(default)]
        align: Align,
        #[serde(default = "default_note_color")]
        color: String,
        #[serde(default = "default_fade")]
        fade_in: u32,
        #[serde(default = "default_fade")]
        fade_out: u32,
    },
    /// A still image decoded from disk and drawn into a layout box.
    Image {
        at: u32,
        dur: u32,
        src: String,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        #[serde(default)]
        fit: Fit,
        #[serde(default = "default_one")]
        opacity: f32,
        #[serde(default)]
        radius: f32,
        #[serde(default = "default_fade")]
        fade_in: u32,
        #[serde(default = "default_fade")]
        fade_out: u32,
        /// A required source must decode; an optional one is a documented
        /// reserved slot and may stay absent.
        #[serde(default = "default_required")]
        required: bool,
    },
    /// A video stream decoded on demand; `src_in` is the first source frame.
    Video {
        at: u32,
        dur: u32,
        src: String,
        #[serde(default)]
        src_in: u32,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        #[serde(default)]
        fit: Fit,
        #[serde(default = "default_one")]
        opacity: f32,
        #[serde(default)]
        radius: f32,
        #[serde(default = "default_fade")]
        fade_in: u32,
        #[serde(default = "default_fade")]
        fade_out: u32,
        /// See [`Item::Image`]: required sources must decode.
        #[serde(default = "default_required")]
        required: bool,
    },
    Progress {
        at: u32,
        dur: u32,
        #[serde(default)]
        track: Option<String>,
        #[serde(default)]
        fill: Option<String>,
        #[serde(default)]
        y: Option<f32>,
    },
    /// A data chart: bar and polyline series inside one plot box, revealed
    /// along the category axis by `progress`. `(x, y)` is the box center,
    /// `(w, h)` its size, all normalized. Colors accept `$role` tokens.
    Chart {
        at: u32,
        dur: u32,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        #[serde(default = "default_fade")]
        fade_in: u32,
        #[serde(default = "default_fade")]
        fade_out: u32,
        /// Value range mapped onto the box height.
        #[serde(default = "default_domain")]
        domain: [f32; 2],
        #[serde(default)]
        bars: Vec<BarSeries>,
        #[serde(default)]
        lines: Vec<LineSeries>,
        /// Axis line color; `None` paints no axes.
        #[serde(default)]
        axis: Option<String>,
        /// Horizontal grid line color; `None` paints no grid.
        #[serde(default)]
        grid: Option<String>,
        #[serde(default)]
        y_ticks: Vec<YTick>,
        #[serde(default)]
        x_labels: Vec<String>,
        #[serde(default = "default_label_size")]
        label_size: f32,
        /// Reveal fraction along the category axis, `0.0..=1.0`.
        #[serde(default = "default_one_f32")]
        progress: f32,
        /// Paint each bar's value above it.
        #[serde(default)]
        value_labels: bool,
        /// Fixed number of decimals for value labels; the default trims
        /// trailing zeros.
        #[serde(default)]
        value_decimals: Option<u32>,
    },
    /// An annotation: a leader line from an anchor point to a label box.
    /// `progress` draws the leader first, then fades the label in.
    Callout {
        at: u32,
        dur: u32,
        /// Anchor point the leader starts from, normalized.
        ax: f32,
        ay: f32,
        /// Label box center, normalized.
        x: f32,
        y: f32,
        /// Primary label line.
        text: String,
        /// Optional secondary line painted under `text`.
        #[serde(default)]
        note: Option<String>,
        #[serde(default = "default_note_size")]
        size: f32,
        /// Leader, dot and default text color.
        #[serde(default)]
        color: Option<String>,
        /// Label text color; defaults to `color` then a neutral ink.
        #[serde(default)]
        text_color: Option<String>,
        /// Label background panel; `None` paints bare text.
        #[serde(default)]
        panel: Option<String>,
        /// `true` bends the leader with an elbow, `false` draws it straight.
        #[serde(default = "default_true")]
        elbow: bool,
        #[serde(default = "default_one_f32")]
        progress: f32,
        #[serde(default = "default_fade")]
        fade_in: u32,
        #[serde(default = "default_fade")]
        fade_out: u32,
    },
}

impl Item {
    /// Half-open window `[at, at + dur)` this item occupies in its scene.
    pub fn window(&self) -> (u32, u32) {
        match self {
            Item::Text { at, dur, .. }
            | Item::Code { at, dur, .. }
            | Item::Counter { at, dur, .. }
            | Item::Card { at, dur, .. }
            | Item::Shape { at, dur, .. }
            | Item::Bands { at, dur, .. }
            | Item::Grain { at, dur, .. }
            | Item::Path { at, dur, .. }
            | Item::Layer { at, dur, .. }
            | Item::Instance { at, dur, .. }
            | Item::Glow { at, dur, .. }
            | Item::Footnote { at, dur, .. }
            | Item::Image { at, dur, .. }
            | Item::Video { at, dur, .. }
            | Item::Progress { at, dur, .. }
            | Item::Chart { at, dur, .. }
            | Item::Callout { at, dur, .. } => (*at, *at + *dur),
        }
    }

    /// The static anchor point this item paints around in normalized frame
    /// coordinates, when it has one. Full-frame effects, nested layers and
    /// items without a layout position report `None`, so a spatial check can
    /// skip them instead of inventing a position they never paint at.
    pub fn anchor(&self) -> Option<(f32, f32)> {
        match self {
            Item::Text { x, y, .. }
            | Item::Code { x, y, .. }
            | Item::Counter { x, y, .. }
            | Item::Card { x, y, .. }
            | Item::Shape { x, y, .. }
            | Item::Instance { x, y, .. }
            | Item::Glow { x, y, .. }
            | Item::Footnote { x, y, .. }
            | Item::Image { x, y, .. }
            | Item::Video { x, y, .. }
            | Item::Chart { x, y, .. }
            | Item::Callout { x, y, .. } => Some((*x, *y)),
            Item::Path { points, .. } => {
                if points.is_empty() {
                    return None;
                }
                let sum = points.iter().fold((0.0_f32, 0.0_f32), |acc, [x, y]| (acc.0 + x, acc.1 + y));
                Some((sum.0 / points.len() as f32, sum.1 / points.len() as f32))
            }
            Item::Bands { .. } | Item::Grain { .. } | Item::Layer { .. } | Item::Progress { .. } => None,
        }
    }
}

/// One bar series of an [`Item::Chart`].
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BarSeries {
    /// Values in category order; negative values grow below the baseline.
    pub values: Vec<f32>,
    /// Series color, or a `$role` design token.
    pub color: String,
}

/// One polyline series of an [`Item::Chart`].
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LineSeries {
    /// Values in category order.
    pub points: Vec<f32>,
    /// Series color, or a `$role` design token.
    pub color: String,
    /// Stroke width in output pixels at scale 1.
    #[serde(default = "default_stroke")]
    pub stroke: f32,
}

/// One labeled tick on the value axis of an [`Item::Chart`].
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct YTick {
    /// The data value the tick marks.
    pub value: f32,
    /// Text painted beside the tick.
    pub label: String,
}

fn default_domain() -> [f32; 2] {
    [0.0, 1.0]
}

fn default_label_size() -> f32 {
    22.0
}

fn default_stroke() -> f32 {
    3.0
}

fn default_note_size() -> f32 {
    26.0
}

fn default_note_color() -> String {
    "#5d6f8c".to_string()
}

fn default_glow_color() -> String {
    "#f2c98a".to_string()
}

/// Audio bed specification; everything is synthesized in-process except the
/// reserved narration slot which mixes a PCM WAV file when provided.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AudioSpec {
    #[serde(default)]
    pub bgm: Option<BgmSpec>,
    #[serde(default)]
    pub narration: Option<NarrationSpec>,
    #[serde(default)]
    pub sfx: Vec<SfxSpec>,
    /// Synthesized voice placed on the timeline (the singing half of the pair).
    #[serde(default)]
    pub voice: Vec<VoicePlacementSpec>,
    /// One-shot instrument notes placed on the timeline (drums, bass, pad, …).
    #[serde(default)]
    pub notes: Vec<NoteSpec>,
    /// Drum grooves placed on the timeline.
    #[serde(default)]
    pub patterns: Vec<PatternSpec>,
    /// Imported audio placed on the timeline, mixed over the synthesized bed.
    #[serde(default)]
    pub clips: Vec<AudioClipSpec>,
}

/// One imported audio file placed at `at` frames, playing from `offset`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AudioClipSpec {
    pub src: String,
    #[serde(default)]
    pub at: u32,
    /// Length in frames; omitted means "until the file ends".
    #[serde(default)]
    pub dur: Option<u32>,
    /// Source offset in frames skipped before playback starts.
    #[serde(default)]
    pub offset_frames: u32,
    #[serde(default = "default_one")]
    pub volume: f32,
    /// Imported audio is a required input unless the author declares it as an
    /// optional reserved slot.
    #[serde(default = "default_required")]
    pub required: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BgmSpec {
    #[serde(default = "default_bgm_volume")]
    pub volume: f32,
    /// Reserved for future moods; only `pad` exists today.
    #[serde(default = "default_bgm_mood")]
    pub mood: String,
}

fn default_bgm_volume() -> f32 {
    0.16
}

fn default_bgm_mood() -> String {
    "pad".to_string()
}

/// Narration is a slot, not a generator: OpenCut mixes the file you provide.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NarrationSpec {
    pub wav: String,
    #[serde(default)]
    pub delay_frames: u32,
    #[serde(default = "default_one")]
    pub volume: f32,
}

fn default_one() -> f32 {
    1.0
}

fn default_required() -> bool {
    true
}

/// One synthesized effect placed on the bed.
///
/// Either `kind` (a canonical family name) or `preset` (a shared catalogue
/// name) selects the sound; both together is an input-contract violation, not
/// a silent priority rule. The optional parameter fields override the resolved
/// base spec, so authoring can start from a preset and adjust one control.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct SfxSpec {
    pub at: u32,
    #[serde(default = "default_sfx_volume")]
    pub volume: f32,
    /// Canonical family name (`whoosh` … `sparkle`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Named shared preset (`impact-deep`, `riser-tension`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Length in seconds; omitted means the family/preset default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f32>,
    /// Pitch multiplier, 1.0 is the base value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pitch: Option<f32>,
    /// 0 dark … 1 bright.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brightness: Option<f32>,
    /// Output gain before the placement volume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intensity: Option<f32>,
    /// Stereo position, -1 left … 1 right.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pan: Option<f32>,
    /// Deterministic seed for the sound's noise streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u32>,
}

/// One synthesized song placed at `at` frames.
///
/// The song itself is the `media.voice` contract (syllables, tempo, vibrato,
/// breath, volume, seed), unchanged: the film IR carries the same snake_case
/// wire shape and rejects unknown fields.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct VoicePlacementSpec {
    pub at: u32,
    /// Track gain applied on top of the song's own `volume`.
    #[serde(default = "default_one")]
    pub volume: f32,
    /// The song to synthesize.
    pub song: media_voice::VoiceSpec,
}

/// One instrument hit placed at `at` frames.
///
/// `instrument` is a canonical kebab-case name resolved by `media.audio`
/// through the shared instrument catalogue; unknown names are explicit errors.
/// `note` is a MIDI number (A4 = 69), `velocity` the hit gain.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct NoteSpec {
    pub at: u32,
    /// Canonical instrument name (`kick`, `snare`, `bass`, `pad`, …).
    pub instrument: String,
    /// MIDI note number, 21..108 (percusssion ignores it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<u8>,
    /// Length in seconds; omitted means the instrument's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f32>,
    /// Hit gain, 0..1.
    #[serde(default = "default_one")]
    pub velocity: f32,
    /// Brightness, 0..1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<f32>,
    /// Tail length, 0..1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decay: Option<f32>,
    /// Stereo position, -1..1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pan: Option<f32>,
    /// Deterministic seed for the instrument's noise streams.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u32>,
}

/// One drum groove placed at `at` frames.
///
/// Name either `preset` (a shipped groove) or `tracks` (a custom pattern) —
/// both together is an input-contract violation, resolved in `media.audio`.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct PatternSpec {
    pub at: u32,
    /// Shipped groove name (`four-on-floor`, `breakbeat`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Tempo override in BPM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bpm: Option<f32>,
    /// Bar count override (the pattern repeats).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bars: Option<u32>,
    /// Custom tracks, used when no preset is named.
    #[serde(default)]
    pub tracks: Vec<PatternTrackSpec>,
    /// Track gain on top of each step velocity.
    #[serde(default = "default_one")]
    pub volume: f32,
}

/// One custom groove track: a step-velocity list for one instrument.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct PatternTrackSpec {
    pub instrument: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<u8>,
    /// Step velocities, 0 is a rest.
    pub steps: Vec<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decay: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pan: Option<f32>,
}

fn default_sfx_volume() -> f32 {
    0.5
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully declared chart survives the wire round trip unchanged.
    #[test]
    fn chart_wire_round_trip() {
        let json = serde_json::json!({
            "kind": "chart", "at": 0, "dur": 30,
            "x": 0.5, "y": 0.5, "w": 0.75, "h": 0.625,
            "fade_in": 8, "fade_out": 8,
            "domain": [0.0, 2.0],
            "bars": [{ "values": [1.0, 2.0], "color": "#7aa2ff" }],
            "lines": [{ "points": [0.5, 1.5], "color": "#e8b86d", "stroke": 4.0 }],
            "axis": "#5d6f8c", "grid": "#22304c",
            "y_ticks": [{ "value": 1.0, "label": "1" }],
            "x_labels": ["a", "b"],
            "label_size": 18.0,
            "progress": 0.5,
            "value_labels": true,
            "value_decimals": 2
        });
        let item: Item = serde_json::from_value(json.clone()).expect("chart parses");
        let back = serde_json::to_value(&item).expect("chart serializes");
        assert_eq!(json, back);
    }

    /// A chart with only the box and timing takes every documented default.
    #[test]
    fn chart_defaults() {
        let minimal = serde_json::json!({
            "kind": "chart", "at": 0, "dur": 30,
            "x": 0.5, "y": 0.5, "w": 0.8, "h": 0.6
        });
        let item: Item = serde_json::from_value(minimal).expect("minimal chart parses");
        let back = serde_json::to_value(&item).expect("chart serializes");
        assert_eq!(back["domain"], serde_json::json!([0.0, 1.0]));
        assert_eq!(back["fade_in"], serde_json::json!(10));
        assert_eq!(back["fade_out"], serde_json::json!(10));
        assert_eq!(back["label_size"], serde_json::json!(22.0));
        assert_eq!(back["progress"], serde_json::json!(1.0));
        assert_eq!(back["bars"], serde_json::json!([]));
        assert_eq!(back["lines"], serde_json::json!([]));
        assert_eq!(back["y_ticks"], serde_json::json!([]));
        assert_eq!(back["x_labels"], serde_json::json!([]));
        assert!(back["axis"].is_null());
        assert!(back["grid"].is_null());
        assert_eq!(back["value_labels"], serde_json::json!(false));
        assert!(back["value_decimals"].is_null());
    }

    /// A line series omits its stroke width independently of the color.
    #[test]
    fn line_series_defaults_stroke() {
        let series: LineSeries =
            serde_json::from_value(serde_json::json!({ "points": [0.1], "color": "#ffffff" })).expect("line series parses");
        assert_eq!(series.stroke, 3.0);
    }

    /// A fully declared callout survives the wire round trip unchanged.
    #[test]
    fn callout_wire_round_trip() {
        let json = serde_json::json!({
            "kind": "callout", "at": 4, "dur": 40,
            "ax": 0.25, "ay": 0.75, "x": 0.5, "y": 0.25,
            "text": "960 GB/s", "note": "measured",
            "size": 30.0, "color": "#e8b86d", "text_color": "#0d1526",
            "panel": "#e8b86d", "elbow": false, "progress": 0.5,
            "fade_in": 6, "fade_out": 6
        });
        let item: Item = serde_json::from_value(json.clone()).expect("callout parses");
        let back = serde_json::to_value(&item).expect("callout serializes");
        assert_eq!(json, back);
    }

    /// A callout with only the anchor, label and timing takes the documented
    /// defaults: an elbow leader at full reveal.
    #[test]
    fn callout_defaults() {
        let minimal = serde_json::json!({
            "kind": "callout", "at": 0, "dur": 30,
            "ax": 0.2, "ay": 0.7, "x": 0.6, "y": 0.3, "text": "note"
        });
        let item: Item = serde_json::from_value(minimal).expect("minimal callout parses");
        let back = serde_json::to_value(&item).expect("callout serializes");
        assert_eq!(back["elbow"], serde_json::json!(true));
        assert_eq!(back["progress"], serde_json::json!(1.0));
        assert_eq!(back["size"], serde_json::json!(26.0));
        assert_eq!(back["fade_in"], serde_json::json!(10));
        assert_eq!(back["fade_out"], serde_json::json!(10));
        assert!(back["note"].is_null());
        assert!(back["color"].is_null());
        assert!(back["text_color"].is_null());
        assert!(back["panel"].is_null());
    }
}
