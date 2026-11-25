//! Editable authored deck model.
//!
//! This module is the authoring truth of a presentation. Playback and rendering
//! consume a projection of a committed revision; they never write back into the
//! authored document. Chart, formula and media semantics stay in their own
//! upstream trees and are referenced from here.
//!
//! Units are EMU. The TypeScript authoring surface converts points once at the
//! boundary, so the model never mixes screen pixels with layout points.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{
    error::{DeckError, codes, params::ObjectKind},
    id::{AssetId, DeckId, ElementId, IdAllocator, LayoutId, MasterId, SlideId, ThemeId},
    size::{DeckSize, points_to_emu},
};

/// Marker written into the canonical project JSON.
pub const PROJECT_FORMAT: &str = "open-ppt/deck";
/// Container contract version understood by this build.
pub const PROJECT_VERSION: u32 = 0;

/// Placement of an element on its slide, in EMU.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transform {
    /// Left edge in EMU, measured from the slide left.
    pub x_emu: i64,
    /// Top edge in EMU, measured from the slide top.
    pub y_emu: i64,
    /// Box width in EMU.
    pub width_emu: i64,
    /// Box height in EMU.
    pub height_emu: i64,
    /// Clockwise rotation in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation_deg: Option<f64>,
}

impl Transform {
    /// Builds a transform from point values, the unit of the authoring surface.
    pub fn from_points(x_pt: f64, y_pt: f64, width_pt: f64, height_pt: f64) -> Option<Self> {
        Some(Self {
            x_emu: signed_points_to_emu(x_pt)?,
            y_emu: signed_points_to_emu(y_pt)?,
            width_emu: points_to_emu(width_pt)?,
            height_emu: points_to_emu(height_pt)?,
            rotation_deg: None,
        })
    }

    /// Whether the box lies fully inside a slide of `size`.
    pub fn inside(&self, size: DeckSize) -> bool {
        self.x_emu >= 0
            && self.y_emu >= 0
            && self.width_emu >= 0
            && self.height_emu >= 0
            && self.x_emu.saturating_add(self.width_emu) <= size.width_emu
            && self.y_emu.saturating_add(self.height_emu) <= size.height_emu
    }
}

fn signed_points_to_emu(points: f64) -> Option<i64> {
    if !points.is_finite() {
        return None;
    }
    let magnitude = if points == 0.0 { 0.0 } else { points.abs() * 12_700.0 };
    if magnitude > i64::MAX as f64 {
        return None;
    }
    let emu = magnitude.round() as i64;
    Some(if points < 0.0 { -emu } else { emu })
}

/// Fill of a shape, background or table cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Fill {
    /// Explicitly no fill.
    None,
    /// Solid colour such as `#1F2937`.
    Solid {
        /// CSS-style colour string kept verbatim.
        color: String,
    },
    /// Inherit the fill from the layout or theme.
    Inherit,
}

/// Stroke of a shape or line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Line {
    /// Stroke colour, or `None` for the theme default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Stroke width in EMU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width_emu: Option<i64>,
}

/// Style properties an element inherits or overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElementStyle {
    /// Fill override, or `None` for inherit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<Fill>,
    /// Line override, or `None` for inherit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<Line>,
    /// Font family override, or `None` for inherit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
}

/// Binding to a layout placeholder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaceholderRef {
    /// Layout that owns the placeholder, when the binding is layout-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<LayoutId>,
    /// Placeholder name such as `title` or `body`.
    pub name: String,
}

/// Editable flags of an element.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LockFlags {
    /// Whether the element may be moved.
    #[serde(default, rename = "move")]
    pub move_: bool,
    /// Whether the element may be resized.
    #[serde(default)]
    pub resize: bool,
    /// Whether the element style may be changed.
    #[serde(default)]
    pub style: bool,
    /// Whether the element may be removed.
    #[serde(default)]
    pub remove: bool,
}

/// Where an element came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OriginKind {
    /// Created through the authoring surface.
    Authoring,
    /// Produced by an importer.
    Import,
}

/// Provenance kept for loss reports and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ElementOrigin {
    /// Authoring or import origin.
    pub kind: OriginKind,
    /// Source part inside the imported package, when any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part: Option<String>,
}

/// Element categories the model distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ElementKind {
    /// Rich text box or placeholder.
    Text,
    /// Raster or vector image.
    Image,
    /// Preset shape.
    Shape,
    /// Group of elements; declared but not editable yet.
    Group,
    /// Table.
    Table,
    /// Chart kept as editable data.
    Chart,
    /// Formula kept as source expression.
    Formula,
    /// Audio or video.
    Media,
}

impl ElementKind {
    /// Stable kebab-case name used by diagnostics and the authoring surface.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Shape => "shape",
            Self::Group => "group",
            Self::Table => "table",
            Self::Chart => "chart",
            Self::Formula => "formula",
            Self::Media => "media",
        }
    }
}

/// Paragraph alignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TextAlign {
    /// Left aligned.
    Left,
    /// Centered.
    Center,
    /// Right aligned.
    Right,
    /// Justified.
    Justify,
}

/// One styled run inside a paragraph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextRun {
    /// Run text.
    pub text: String,
    /// Bold flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    /// Italic flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub italic: Option<bool>,
    /// Underline flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub underline: Option<bool>,
    /// Strikethrough flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strike: Option<bool>,
    /// Font size in points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_pt: Option<f64>,
    /// Text colour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Font family.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// Hyperlink destination kept as authored text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hyperlink: Option<String>,
}

impl TextRun {
    /// Builds an unstyled run.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            bold: None,
            italic: None,
            underline: None,
            strike: None,
            size_pt: None,
            color: None,
            font_family: None,
            hyperlink: None,
        }
    }
}

/// One paragraph of rich text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextParagraph {
    /// Runs in reading order.
    pub runs: Vec<TextRun>,
    /// Outline level, where `0` is the top level.
    #[serde(default)]
    pub level: u8,
    /// Paragraph alignment override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<TextAlign>,
}

impl TextParagraph {
    /// Concatenates the run texts.
    pub fn text(&self) -> String {
        self.runs.iter().map(|run| run.text.as_str()).collect()
    }
}

/// Rich text content of a text element.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextContent {
    /// Paragraphs in reading order.
    pub paragraphs: Vec<TextParagraph>,
}

impl TextContent {
    /// Builds one paragraph from plain text, splitting on newlines.
    pub fn plain(text: impl Into<String>) -> Self {
        let text = text.into();
        let paragraphs = text.split('\n').map(|line| TextParagraph { runs: vec![TextRun::plain(line)], level: 0, align: None }).collect();
        Self { paragraphs }
    }

    /// Joins paragraphs with newlines, the form `slide.title` reports.
    pub fn text(&self) -> String {
        self.paragraphs.iter().map(TextParagraph::text).collect::<Vec<_>>().join("\n")
    }
}

/// Image crop expressed as fractions of the source bounds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Crop {
    /// Fraction removed from the left edge.
    pub left: f64,
    /// Fraction removed from the top edge.
    pub top: f64,
    /// Fraction removed from the right edge.
    pub right: f64,
    /// Fraction removed from the bottom edge.
    pub bottom: f64,
}

/// Image element content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    /// Referenced asset.
    pub asset: AssetId,
    /// Optional crop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<Crop>,
}

/// Preset shape content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShapeContent {
    /// Preset geometry name such as `rect` or `roundRect`.
    pub geometry: String,
    /// Fill override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<Fill>,
    /// Line override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<Line>,
}

/// One table cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableCell {
    /// Cell rich text.
    pub text: TextContent,
    /// Cell fill override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fill: Option<Fill>,
    /// Number of columns the cell spans.
    #[serde(default = "one")]
    pub column_span: u32,
    /// Number of rows the cell spans.
    #[serde(default = "one")]
    pub row_span: u32,
}

fn one() -> u32 {
    1
}

impl TableCell {
    /// Builds a single-span cell from plain text.
    pub fn plain(text: impl Into<String>) -> Self {
        Self { text: TextContent::plain(text), fill: None, column_span: 1, row_span: 1 }
    }
}

/// One table row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableRow {
    /// Row height in EMU.
    pub height_emu: i64,
    /// Cells in column order.
    pub cells: Vec<TableCell>,
}

/// Table element content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableContent {
    /// Column widths in EMU.
    pub column_widths_emu: Vec<i64>,
    /// Rows in reading order.
    pub rows: Vec<TableRow>,
}

/// Chart families kept editable as data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChartType {
    /// Vertical columns.
    Column,
    /// Horizontal bars.
    Bar,
    /// Line chart.
    Line,
    /// Area chart.
    Area,
    /// Pie chart.
    Pie,
    /// Doughnut chart.
    Doughnut,
    /// Scatter plot.
    Scatter,
    /// Radar chart.
    Radar,
}

/// One chart series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChartSeries {
    /// Series name.
    pub name: String,
    /// Values aligned to the categories.
    pub values: Vec<f64>,
}

/// Editable chart data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChartData {
    /// Category labels.
    pub categories: Vec<String>,
    /// Series in legend order.
    pub series: Vec<ChartSeries>,
}

/// Chart element content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChartContent {
    /// Chart family.
    pub chart_type: ChartType,
    /// Editable data, never a rendered bitmap.
    pub data: ChartData,
}

/// Formula element content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormulaContent {
    /// Source expression such as `E = mc^2`.
    pub expression: String,
}

/// Media family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MediaKind {
    /// Audio clip.
    Audio,
    /// Video clip.
    Video,
}

/// Media element content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaContent {
    /// Audio or video.
    pub media_kind: MediaKind,
    /// Referenced asset.
    pub asset: AssetId,
}

/// Typed content of an element.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "kebab-case")]
pub enum ElementContent {
    /// Rich text.
    Text(TextContent),
    /// Image.
    Image(ImageContent),
    /// Preset shape.
    Shape(ShapeContent),
    /// Table.
    Table(TableContent),
    /// Chart.
    Chart(ChartContent),
    /// Formula.
    Formula(FormulaContent),
    /// Audio or video.
    Media(MediaContent),
}

impl ElementContent {
    /// Category of this content.
    pub fn kind(&self) -> ElementKind {
        match self {
            Self::Text(_) => ElementKind::Text,
            Self::Image(_) => ElementKind::Image,
            Self::Shape(_) => ElementKind::Shape,
            Self::Table(_) => ElementKind::Table,
            Self::Chart(_) => ElementKind::Chart,
            Self::Formula(_) => ElementKind::Formula,
            Self::Media(_) => ElementKind::Media,
        }
    }
}

/// One editable object on a slide.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Element {
    /// Stable identity.
    pub id: ElementId,
    /// Category, mirrored from `content`.
    pub kind: ElementKind,
    /// Placement in EMU.
    pub transform: Transform,
    /// Layout placeholder binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<PlaceholderRef>,
    /// Style overrides.
    #[serde(default)]
    pub style: ElementStyle,
    /// Typed semantic content.
    pub content: ElementContent,
    /// Alternative text for accessibility and diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
    /// Lock flags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<LockFlags>,
    /// Provenance for loss reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ElementOrigin>,
}

impl Element {
    /// Builds an element whose category is derived from its content.
    pub fn new(id: ElementId, transform: Transform, content: ElementContent) -> Self {
        let kind = content.kind();
        Self { id, kind, transform, placeholder: None, style: ElementStyle::default(), content, alt: None, lock: None, source: None }
    }
}

/// One slide of the deck.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeckSlide {
    /// Stable identity; the index is order only.
    pub id: SlideId,
    /// Applied layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<LayoutId>,
    /// Hidden slides are skipped by playback and preserved on save.
    #[serde(default)]
    pub hidden: bool,
    /// Slide background override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<Fill>,
    /// Elements in paint order.
    pub elements: Vec<Element>,
    /// Speaker notes kept as plain text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl DeckSlide {
    /// Finds one element by identity.
    pub fn element(&self, id: &ElementId) -> Option<&Element> {
        self.elements.iter().find(|element| &element.id == id)
    }

    /// Finds one element by identity for editing.
    pub fn element_mut(&mut self, id: &ElementId) -> Option<&mut Element> {
        self.elements.iter_mut().find(|element| &element.id == id)
    }

    /// Returns the element bound to the `title` placeholder, when present.
    pub fn title_element(&self) -> Option<&Element> {
        self.elements.iter().find(|element| {
            element.kind == ElementKind::Text && element.placeholder.as_ref().is_some_and(|placeholder| placeholder.name == "title")
        })
    }

    /// Plain text of the title placeholder, or an empty string.
    pub fn title(&self) -> String {
        self.title_element()
            .and_then(|element| match &element.content {
                ElementContent::Text(text) => Some(text.text()),
                _ => None,
            })
            .unwrap_or_default()
    }
}

/// One theme.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Theme {
    /// Stable identity.
    pub id: ThemeId,
    /// Display name.
    pub name: String,
    /// Fonts by role such as `heading` or `body`.
    #[serde(default)]
    pub fonts: BTreeMap<String, String>,
    /// Colour tokens such as `accent1`.
    #[serde(default)]
    pub colors: BTreeMap<String, String>,
}

/// One master.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Master {
    /// Stable identity.
    pub id: MasterId,
    /// Display name.
    pub name: String,
    /// Layouts owned by this master.
    #[serde(default)]
    pub layout_ids: Vec<LayoutId>,
}

/// One layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Layout {
    /// Stable identity.
    pub id: LayoutId,
    /// Display name.
    pub name: String,
    /// Owning master.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master: Option<MasterId>,
    /// Placeholder names this layout declares.
    #[serde(default)]
    pub placeholders: Vec<String>,
}

/// One embedded asset record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Asset {
    /// Stable identity.
    pub id: AssetId,
    /// Original file name.
    pub name: String,
    /// MIME type.
    pub media_type: String,
    /// Lowercase hex SHA-256 of the bytes.
    pub sha256: String,
    /// Licence or provenance note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Embedded bytes; stored in the project archive rather than the JSON.
    #[serde(skip)]
    pub bytes: Vec<u8>,
}

/// Canonical authored document, the part persisted in `deck.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeckDocument {
    /// Contract marker; must equal [`PROJECT_FORMAT`].
    #[serde(default = "project_format")]
    pub format: String,
    /// Contract version; must equal [`PROJECT_VERSION`].
    #[serde(default = "project_version")]
    pub version: u32,
    /// Stable deck identity.
    pub id: DeckId,
    /// Monotonic revision, incremented on every commit.
    pub revision: u64,
    /// Identity allocator state.
    pub next_object_id: IdAllocator,
    /// Optional deck title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Slide extent in EMU.
    pub size: DeckSize,
    /// Active theme.
    pub theme: ThemeId,
    /// Themes declared by the deck.
    pub themes: Vec<Theme>,
    /// Masters declared by the deck.
    pub masters: Vec<Master>,
    /// Layouts declared by the deck.
    pub layouts: Vec<Layout>,
    /// Slides in playback order.
    pub slides: Vec<DeckSlide>,
    /// Embedded assets.
    pub assets: Vec<Asset>,
}

fn project_format() -> String {
    PROJECT_FORMAT.to_string()
}

fn project_version() -> u32 {
    PROJECT_VERSION
}

impl DeckDocument {
    /// Finds a slide by identity.
    pub fn slide(&self, id: &SlideId) -> Option<&DeckSlide> {
        self.slides.iter().find(|slide| &slide.id == id)
    }

    /// Finds an asset by identity.
    pub fn asset(&self, id: &AssetId) -> Option<&Asset> {
        self.assets.iter().find(|asset| &asset.id == id)
    }

    /// Checks structural invariants written by an importer or an earlier build.
    pub fn validate(&self) -> Result<(), DeckError> {
        if self.format != PROJECT_FORMAT {
            return Err(DeckError::format_unsupported(self.format.clone()));
        }
        if self.version != PROJECT_VERSION {
            let mut error = DeckError::new(
                codes::FORMAT_UNSUPPORTED,
                format!("deck contract version {} is not supported", self.version),
            );
            error = error.with_format(format!("contract-v{}", self.version));
            return Err(error);
        }

        let mut identities = BTreeSet::new();
        let mut unique = |identity: &str| identities.insert(identity.to_string());
        unique(self.id.as_str());
        for theme in &self.themes {
            unique(theme.id.as_str());
        }
        for master in &self.masters {
            unique(master.id.as_str());
        }
        for layout in &self.layouts {
            unique(layout.id.as_str());
        }
        for slide in &self.slides {
            unique(slide.id.as_str());
            for element in &slide.elements {
                unique(element.id.as_str());
            }
        }
        for asset in &self.assets {
            unique(asset.id.as_str());
        }
        if identities.len()
            != 1 + self.themes.len()
                + self.masters.len()
                + self.layouts.len()
                + self.slides.len()
                + self.slides.iter().map(|slide| slide.elements.len()).sum::<usize>()
                + self.assets.len()
        {
            return Err(DeckError::new(codes::FORMAT_INVALID_PACKAGE, "object identities are not unique inside the deck"));
        }
        let highest_allocated = identities
            .iter()
            .filter_map(|identity| {
                let digits: String = identity.chars().rev().take_while(char::is_ascii_digit).collect();
                digits.chars().rev().collect::<String>().parse::<u64>().ok()
            })
            .max()
            .unwrap_or(0);
        if self.next_object_id.counter() < highest_allocated {
            return Err(DeckError::new(
                codes::FORMAT_INVALID_PACKAGE,
                "the identity allocator is behind identities already present in the deck",
            ));
        }

        if !self.themes.iter().any(|theme| theme.id == self.theme) {
            return Err(DeckError::new(codes::FORMAT_INVALID_PACKAGE, "deck theme is not declared"));
        }
        let layout_ids: BTreeSet<&LayoutId> = self.layouts.iter().map(|layout| &layout.id).collect();
        let master_ids: BTreeSet<&MasterId> = self.masters.iter().map(|master| &master.id).collect();
        let asset_ids: BTreeSet<&AssetId> = self.assets.iter().map(|asset| &asset.id).collect();
        for master in &self.masters {
            if let Some(missing) = master.layout_ids.iter().find(|layout| !layout_ids.contains(layout)) {
                return Err(DeckError::new(codes::FORMAT_INVALID_PACKAGE, format!("master references unknown layout `{missing}`")));
            }
        }
        for layout in &self.layouts {
            if let Some(master) = &layout.master
                && !master_ids.contains(master)
            {
                return Err(DeckError::new(codes::FORMAT_INVALID_PACKAGE, format!("layout references unknown master `{master}`")));
            }
        }
        for slide in &self.slides {
            if let Some(layout) = &slide.layout
                && !layout_ids.contains(layout)
            {
                return Err(DeckError::new(codes::FORMAT_INVALID_PACKAGE, format!("slide references unknown layout `{layout}`"))
                    .with_slide(slide.id.clone()));
            }
            for element in &slide.elements {
                if element.kind != element.content.kind() {
                    return Err(DeckError::new(codes::FORMAT_INVALID_PACKAGE, "element kind does not match its content")
                        .with_slide(slide.id.clone())
                        .with_element(element.id.clone()));
                }
                let referenced = match &element.content {
                    ElementContent::Image(image) => Some(&image.asset),
                    ElementContent::Media(media) => Some(&media.asset),
                    _ => None,
                };
                if let Some(asset) = referenced
                    && !asset_ids.contains(asset)
                {
                    return Err(DeckError::unknown(ObjectKind::Asset, asset.as_str())
                        .with_slide(slide.id.clone())
                        .with_element(element.id.clone()));
                }
            }
        }
        Ok(())
    }
}

/// What `Deck::create` accepts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeckCreateOptions {
    /// Deck title.
    pub title: Option<String>,
    /// Slide extent; defaults to `16:9`.
    pub size: Option<DeckSize>,
    /// Theme display name; a default theme is used otherwise.
    pub theme_name: Option<String>,
}

/// An editable deck with its undo and redo stacks.
///
/// The stacks hold committed document snapshots and never enter the saved
/// project, so reopening a deck starts with an empty history.
#[derive(Debug, Clone)]
pub struct Deck {
    document: DeckDocument,
    undo: Vec<DeckDocument>,
    redo: Vec<DeckDocument>,
}

/// Maximum number of snapshots kept in the undo stack.
pub const HISTORY_LIMIT: usize = 64;

impl Deck {
    /// Creates an empty deck with one default theme, master and layout pair.
    pub fn create(options: DeckCreateOptions) -> Self {
        let mut ids = IdAllocator::new();
        let deck_id = DeckId::new(ids.allocate("deck")).expect("allocated identity is non-empty");
        let theme_id = ThemeId::new(ids.allocate("theme")).expect("allocated identity is non-empty");
        let master_id = MasterId::new(ids.allocate("master")).expect("allocated identity is non-empty");
        let title_layout = LayoutId::new(ids.allocate("layout")).expect("allocated identity is non-empty");
        let content_layout = LayoutId::new(ids.allocate("layout")).expect("allocated identity is non-empty");

        let theme = Theme {
            id: theme_id.clone(),
            name: options.theme_name.unwrap_or_else(|| "default".to_string()),
            fonts: BTreeMap::new(),
            colors: BTreeMap::new(),
        };
        let master =
            Master { id: master_id.clone(), name: "default".to_string(), layout_ids: vec![title_layout.clone(), content_layout.clone()] };
        let layouts = vec![
            Layout { id: title_layout, name: "title".to_string(), master: Some(master_id.clone()), placeholders: vec!["title".to_string()] },
            Layout {
                id: content_layout,
                name: "title-and-content".to_string(),
                master: Some(master_id),
                placeholders: vec!["title".to_string(), "body".to_string()],
            },
        ];

        Self {
            document: DeckDocument {
                format: PROJECT_FORMAT.to_string(),
                version: PROJECT_VERSION,
                id: deck_id,
                revision: 0,
                next_object_id: ids,
                title: options.title,
                size: options.size.unwrap_or(DeckSize::WIDESCREEN),
                theme: theme_id,
                themes: vec![theme],
                masters: vec![master],
                layouts,
                slides: Vec::new(),
                assets: Vec::new(),
            },
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    /// Restores a deck from a decoded document, validating it first.
    pub fn from_document(document: DeckDocument) -> Result<Self, DeckError> {
        document.validate()?;
        Ok(Self { document, undo: Vec::new(), redo: Vec::new() })
    }

    /// Canonical authored document.
    pub fn document(&self) -> &DeckDocument {
        &self.document
    }

    /// Stable deck identity.
    pub fn id(&self) -> &DeckId {
        &self.document.id
    }

    /// Committed revision.
    pub fn revision(&self) -> u64 {
        self.document.revision
    }

    /// Slide extent in EMU.
    pub fn size(&self) -> DeckSize {
        self.document.size
    }

    /// Deck title, when one was set at creation.
    pub fn title(&self) -> Option<&str> {
        self.document.title.as_deref()
    }

    /// Slides in playback order.
    pub fn slides(&self) -> &[DeckSlide] {
        &self.document.slides
    }

    /// Finds one slide by identity.
    pub fn slide(&self, id: &SlideId) -> Option<&DeckSlide> {
        self.document.slide(id)
    }

    /// Position of a slide, or `None` when it is not part of the deck.
    pub fn slide_index(&self, id: &SlideId) -> Option<usize> {
        self.document.slides.iter().position(|slide| &slide.id == id)
    }

    /// Declared masters.
    pub fn masters(&self) -> &[Master] {
        &self.document.masters
    }

    /// Declared layouts.
    pub fn layouts(&self) -> &[Layout] {
        &self.document.layouts
    }

    /// Declared themes.
    pub fn themes(&self) -> &[Theme] {
        &self.document.themes
    }

    /// Active theme identity.
    pub fn theme_id(&self) -> &ThemeId {
        &self.document.theme
    }

    /// Embedded assets.
    pub fn assets(&self) -> &[Asset] {
        &self.document.assets
    }

    /// Finds one asset by identity.
    pub fn asset(&self, id: &AssetId) -> Option<&Asset> {
        self.document.asset(id)
    }

    /// Number of undo steps still available.
    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    /// Number of redo steps still available.
    pub fn redo_depth(&self) -> usize {
        self.redo.len()
    }

    pub(super) fn document_mut(&mut self) -> &mut DeckDocument {
        &mut self.document
    }

    pub(super) fn undo_snapshot(&mut self) -> Option<DeckDocument> {
        self.undo.pop()
    }

    pub(super) fn redo_snapshot(&mut self) -> Option<DeckDocument> {
        self.redo.pop()
    }

    pub(super) fn push_undo(&mut self, document: DeckDocument) {
        self.undo.push(document);
        if self.undo.len() > HISTORY_LIMIT {
            self.undo.remove(0);
        }
    }

    pub(super) fn push_redo(&mut self, document: DeckDocument) {
        self.redo.push(document);
    }

    pub(super) fn commit_document(&mut self, document: DeckDocument) {
        let previous = std::mem::replace(&mut self.document, document);
        self.push_undo(previous);
        self.redo.clear();
    }
}
