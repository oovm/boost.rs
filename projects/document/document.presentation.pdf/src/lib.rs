//! Static PDF projection of the shared editable deck model.
//!
//! The projection lowers one [`Deck`] into a fixed-geometry
//! [`DisplayPresentation`] and hands it to the shared PDF backend, so the deck
//! product keeps only its request plumbing instead of owning a second PDF
//! writer.
//!
//! Backgrounds, preset shapes (rectangles and ellipses), text, tables and
//! pictures are painted. Charts, formulas and media have no static
//! representation and are reported as loss codes. Text paints from the base-14
//! faces in WinAnsi, so characters outside Latin-1 degrade to `?` and are
//! reported. Pictures are embedded through the backend's PNG, JPEG and SVG
//! decoders from asset bytes the caller already owns, so no network or
//! filesystem lookup happens during a projection.
//!
//! ```text
//! let export = export_pdf_bytes(&deck)?;
//! std::fs::write("deck.pdf", &export.bytes)?;
//! ```

#![warn(missing_docs)]

use std::{
    borrow::Cow,
    fmt,
};

use document_compose::math::FontStyle;
use document_presentation::{
    authored::{
        Asset, Deck, DeckSlide, Element, ElementContent, Fill, Line, TableContent, TextAlign, TextParagraph, Transform, emu_to_points,
    },
    display::{
        DisplayEllipse, DisplayImage, DisplayItem, DisplayPage, DisplayPresentation, DisplayRect, DisplayTextRun,
    },
};
use document_renderer_pdf::{PdfFontPolicy, PdfResources, render_pdf_with_resources_and_diagnostics};

/// Loss reported when a picture cannot be embedded.
pub const LOSS_IMAGE: &str = "image";
/// Loss reported when text contains a character outside WinAnsi/Latin-1.
pub const LOSS_GLYPH: &str = "glyph";
/// Loss reported when a chart is skipped.
pub const LOSS_CHART: &str = "chart";
/// Loss reported when a formula is skipped.
pub const LOSS_FORMULA: &str = "formula";
/// Loss reported when audio or video is skipped.
pub const LOSS_MEDIA: &str = "media";
/// Loss reported when a group shape is skipped.
///
/// The authored model declares a group category but has no group content yet,
/// so this code is published for contract parity and stays unused until a
/// group becomes projectable.
pub const LOSS_GROUP: &str = "group";

/// Inner padding of a text box, in points.
const PAD: f64 = 4.0;
/// Default text size when no run declares one, in points.
const DEFAULT_TEXT_PT: f64 = 18.0;
/// Default table cell text size, in points.
const CELL_TEXT_PT: f64 = 10.0;
/// Family name the backend folds onto its bundled prose face.
///
/// The base-14 policy maps a name containing `sans` to Helvetica, which is the
/// face this projection has always painted deck text with.
const TEXT_FAMILY: &str = "sans-serif";

/// Failure of one PDF projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfError {
    /// Stable backend code, such as `vutex.render.pdf.no_pages`.
    pub code: String,
    /// Human-readable reason.
    pub message: String,
}

impl fmt::Display for PdfError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for PdfError {}

/// Loss reported for one construct the projection could not paint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdfLoss {
    /// Stable loss code such as `chart`.
    pub code: &'static str,
    /// Slide identity the construct belonged to.
    pub slide: String,
    /// Element identity the construct belonged to.
    pub element: String,
    /// Human-readable reason.
    pub message: &'static str,
}

/// PDF bytes plus the construct losses the projection recorded.
#[derive(Debug, Clone)]
pub struct PdfExport {
    /// Complete PDF 1.7 bytes ready to write.
    pub bytes: Vec<u8>,
    /// Constructs dropped or degraded while projecting.
    pub losses: Vec<PdfLoss>,
}

/// Projects `deck` to PDF bytes with the base-14 font policy.
pub fn export_pdf_bytes(deck: &Deck) -> Result<PdfExport, PdfError> {
    export_pdf_bytes_with_font_policy(deck, &PdfFontPolicy::Base14Only)
}

/// Projects `deck` to PDF bytes with an explicit font policy.
///
/// Every embedded asset is supplied to the backend under its `asset:<id>`
/// source key, so a picture never falls back to the filesystem.
pub fn export_pdf_bytes_with_font_policy(deck: &Deck, policy: &PdfFontPolicy) -> Result<PdfExport, PdfError> {
    export_pdf_bytes_with_resources(deck, policy, &PdfResources::default())
}

/// Projects `deck` to PDF bytes with an explicit font policy and caller resources.
///
/// Caller-supplied sources are kept, and embedded assets take precedence for the
/// `asset:<id>` keys this projection emits.
pub fn export_pdf_bytes_with_resources(
    deck: &Deck,
    policy: &PdfFontPolicy,
    resources: &PdfResources,
) -> Result<PdfExport, PdfError> {
    let (presentation, losses) = deck_display_presentation(deck);
    let mut resources = resources.clone();
    for asset in deck.assets() {
        resources = resources.with_supplied(asset_source(asset), asset.bytes.clone());
    }
    let outcome = render_pdf_with_resources_and_diagnostics(&presentation, policy, &resources)
        .map_err(|diagnostic| PdfError { code: diagnostic.code, message: diagnostic.message })?;
    Ok(PdfExport { bytes: outcome.pdf, losses })
}

/// Lowers one deck into the fixed-geometry presentation IR, reporting losses.
pub fn deck_display_presentation(deck: &Deck) -> (DisplayPresentation, Vec<PdfLoss>) {
    let size = deck.size();
    let width = emu_to_points(size.width_emu);
    let height = emu_to_points(size.height_emu);
    let mut losses = Vec::new();
    let pages = deck
        .slides()
        .iter()
        .map(|slide| project_slide(deck, slide, width, height, &mut losses))
        .collect();
    (DisplayPresentation { pages }, losses)
}

fn project_slide(deck: &Deck, slide: &DeckSlide, width: f64, height: f64, losses: &mut Vec<PdfLoss>) -> DisplayPage {
    let mut items = Vec::new();
    if let Some(Fill::Solid { color }) = &slide.background
        && is_color(color)
    {
        items.push(DisplayItem::Rect(DisplayRect {
            x: 0.0,
            y: 0.0,
            width,
            height,
            filled: true,
            fill: Some(Cow::Owned(color.clone())),
            stroke: None,
            stroke_width: 0.0,
        }));
    }
    for element in &slide.elements {
        project_element(deck, slide, element, &mut items, losses);
    }
    DisplayPage { width, height, items, anchors: Vec::new() }
}

fn project_element(deck: &Deck, slide: &DeckSlide, element: &Element, items: &mut Vec<DisplayItem>, losses: &mut Vec<PdfLoss>) {
    match &element.content {
        ElementContent::Text(text) => {
            let mut glyph_loss = false;
            paint_text(items, &text.paragraphs, Frame::from(element.transform), DEFAULT_TEXT_PT, &mut glyph_loss);
            if glyph_loss {
                losses.push(loss(slide, element, LOSS_GLYPH, "text outside Latin-1 is written as `?`"));
            }
        }
        ElementContent::Shape(shape) => {
            paint_shape(items, &shape.geometry, shape.fill.as_ref(), shape.line.as_ref(), Frame::from(element.transform));
        }
        ElementContent::Image(image) => {
            let Some(asset) = deck.asset(&image.asset)
            else {
                losses.push(loss(slide, element, LOSS_IMAGE, "the referenced image asset is missing"));
                return;
            };
            if !embeddable(asset) {
                losses.push(loss(slide, element, LOSS_IMAGE, "only JPEG, PNG and SVG pictures are embedded; other encodings are skipped"));
                return;
            }
            let frame = Frame::from(element.transform);
            items.push(DisplayItem::Image(DisplayImage {
                x: frame.x,
                y: frame.y,
                width: frame.w,
                height: frame.h,
                alt: element.alt.clone().unwrap_or_default(),
                source: Some(asset_source(asset)),
            }));
        }
        ElementContent::Table(table) => {
            paint_table(items, table, Frame::from(element.transform), losses, slide, element);
        }
        ElementContent::Chart(_) => {
            losses.push(loss(slide, element, LOSS_CHART, "charts are kept as editable data and are not painted in PDF"));
        }
        ElementContent::Formula(_) => {
            losses.push(loss(slide, element, LOSS_FORMULA, "formulas keep their source expression and are not painted in PDF"));
        }
        ElementContent::Media(_) => {
            losses.push(loss(slide, element, LOSS_MEDIA, "audio and video have no static PDF representation"));
        }
    }
}

fn paint_shape(items: &mut Vec<DisplayItem>, geometry: &str, fill: Option<&Fill>, line: Option<&Line>, frame: Frame) {
    let fill_color = fill.and_then(solid_color);
    let stroke = line.and_then(|line| {
        let color = line.color.as_deref().filter(|color| is_color(color)).unwrap_or("#000000");
        let width = line.width_emu.map(emu_to_points).unwrap_or(1.0).max(0.0);
        (width > 0.0).then_some((color, width))
    });
    if fill_color.is_none() && stroke.is_none() {
        return;
    }
    if geometry == "ellipse" {
        items.push(DisplayItem::Ellipse(DisplayEllipse {
            x: frame.x,
            y: frame.y,
            width: frame.w,
            height: frame.h,
            fill: fill_color.map(|color| Cow::Owned(color.to_string())),
            stroke: stroke.map(|(color, _)| Cow::Owned(color.to_string())),
            stroke_width: stroke.map(|(_, width)| width).unwrap_or(0.0),
        }));
        return;
    }
    items.push(DisplayItem::Rect(DisplayRect {
        x: frame.x,
        y: frame.y,
        width: frame.w,
        height: frame.h,
        filled: fill_color.is_some(),
        fill: fill_color.map(|color| Cow::Owned(color.to_string())),
        stroke: stroke.map(|(color, _)| Cow::Owned(color.to_string())),
        stroke_width: stroke.map(|(_, width)| width).unwrap_or(0.0),
    }));
}

/// Paints one table as cell backgrounds, grid lines and cell text.
fn paint_table(
    items: &mut Vec<DisplayItem>,
    table: &TableContent,
    frame: Frame,
    losses: &mut Vec<PdfLoss>,
    slide: &DeckSlide,
    element: &Element,
) {
    let columns = table.column_widths_emu.len().max(1);
    let default_width = frame.w / columns as f64;
    let mut top = frame.y;
    for row in &table.rows {
        let height = emu_to_points(row.height_emu);
        let mut left = frame.x;
        for (index, cell) in row.cells.iter().enumerate() {
            let width = table.column_widths_emu.get(index).map(|emu| emu_to_points(*emu)).unwrap_or(default_width);
            if let Some(color) = cell.fill.as_ref().and_then(solid_color) {
                items.push(DisplayItem::Rect(DisplayRect {
                    x: left,
                    y: top,
                    width,
                    height,
                    filled: true,
                    fill: Some(Cow::Owned(color.to_string())),
                    stroke: None,
                    stroke_width: 0.0,
                }));
            }
            items.push(DisplayItem::Rect(DisplayRect {
                x: left,
                y: top,
                width,
                height,
                filled: false,
                fill: None,
                stroke: Some(Cow::Borrowed("#000000")),
                stroke_width: 0.5,
            }));
            let mut glyph_loss = false;
            paint_text(items, &cell.text.paragraphs, Frame { x: left, y: top, w: width, h: height }, CELL_TEXT_PT, &mut glyph_loss);
            if glyph_loss {
                losses.push(loss(slide, element, LOSS_GLYPH, "table text outside Latin-1 is written as `?`"));
            }
            left += width;
        }
        top += height;
    }
}

/// Paints one text frame as one display run per authored line.
///
/// One paragraph contributes one run per hard line break. The run takes the
/// paragraph's largest declared size, its first explicit colour and its bold or
/// italic intent, which is the same resolution the previous writer applied.
fn paint_text(items: &mut Vec<DisplayItem>, paragraphs: &[TextParagraph], frame: Frame, default_size: f64, glyph_loss: &mut bool) {
    let mut top = frame.y + PAD;
    for paragraph in paragraphs {
        let size = paragraph.runs.iter().filter_map(|run| run.size_pt).fold(default_size, f64::max);
        let line_height = size * 1.25;
        let bold = paragraph.runs.iter().any(|run| run.bold.unwrap_or(false));
        let italic = paragraph.runs.iter().any(|run| run.italic.unwrap_or(false));
        let style = if bold {
            FontStyle::Bold
        }
        else if italic {
            FontStyle::Italic
        }
        else {
            FontStyle::Normal
        };
        let color = paragraph.runs.iter().find_map(|run| run.color.as_deref()).filter(|color| is_color(color));
        let text = paragraph.runs.iter().map(|run| run.text.as_str()).collect::<String>();
        for line in text.split('\n') {
            if line.chars().any(|character| (character as u32) > 0x7E) {
                *glyph_loss = true;
            }
            let estimated = line.chars().count() as f64 * size * 0.5;
            let x = match paragraph.align.unwrap_or(TextAlign::Left) {
                TextAlign::Center => frame.x + (frame.w - estimated).max(0.0) / 2.0,
                TextAlign::Right => frame.x + (frame.w - estimated - PAD).max(0.0),
                TextAlign::Left | TextAlign::Justify => frame.x + PAD,
            };
            if !line.is_empty() {
                items.push(DisplayItem::TextRun(DisplayTextRun {
                    x,
                    y: top + line_height - size * 0.25,
                    size,
                    width: estimated,
                    font_family: TEXT_FAMILY.to_string(),
                    style,
                    text: line.to_string(),
                    href: None,
                    anchor: None,
                    title: None,
                    color: color.map(|color| Cow::Owned(color.to_string())),
                    numeric: false,
                    artifact: false,
                }));
            }
            top += line_height;
        }
    }
}

fn loss(slide: &DeckSlide, element: &Element, code: &'static str, message: &'static str) -> PdfLoss {
    PdfLoss { code, slide: slide.id.as_str().to_string(), element: element.id.as_str().to_string(), message }
}

/// Source key one embedded asset is supplied under.
fn asset_source(asset: &Asset) -> String {
    format!("asset:{}", asset.id.as_str())
}

fn solid_color(fill: &Fill) -> Option<&str> {
    match fill {
        Fill::Solid { color } if is_color(color) => Some(color),
        Fill::Solid { .. } | Fill::None | Fill::Inherit => None,
    }
}

/// Whether a value is a CSS hex colour this projection can carry.
fn is_color(value: &str) -> bool {
    let trimmed = value.trim().trim_start_matches('#');
    match trimmed.len() {
        3 | 6 | 8 => trimmed.chars().all(|character| character.is_ascii_hexdigit()),
        _ => false,
    }
}

/// Whether the backend has a decoder for this asset.
fn embeddable(asset: &Asset) -> bool {
    let bytes = &asset.bytes;
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") || bytes.starts_with(&[0xFF, 0xD8]) {
        return true;
    }
    if asset.media_type.eq_ignore_ascii_case("image/svg+xml") || asset.name.to_ascii_lowercase().ends_with(".svg") {
        return true;
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
    head.contains("<svg")
}

#[derive(Debug, Clone, Copy)]
struct Frame {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

impl Frame {
    fn from(transform: Transform) -> Self {
        Self {
            x: emu_to_points(transform.x_emu),
            y: emu_to_points(transform.y_emu),
            w: emu_to_points(transform.width_emu),
            h: emu_to_points(transform.height_emu),
        }
    }
}

#[cfg(test)]
mod tests {
    use document_presentation::authored::{DeckCreateOptions, SlideAddOptions, TextContent, TextElementInput, Transform};

    use super::*;

    fn deck_with_text(lines: &[&str]) -> Deck {
        let mut deck = Deck::create(DeckCreateOptions::default());
        deck.transaction(None, |edit| {
            for text in lines {
                let slide = edit.add_slide(SlideAddOptions { layout: None, index: None })?;
                edit.add_text(
                    &slide,
                    TextElementInput {
                        text: TextContent::plain(*text),
                        transform: Some(Transform::from_points(40.0, 40.0, 400.0, 100.0).unwrap()),
                        alt: None,
                        placeholder: None,
                    },
                )?;
            }
            Ok(())
        })
        .expect("deck builds");
        deck
    }

    fn count(haystack: &[u8], needle: &[u8]) -> usize {
        haystack.windows(needle.len()).filter(|window| *window == needle).count()
    }

    /// Smoke: a text-only deck pages one slide at a time and reports no loss.
    #[test]
    fn a_text_deck_renders_one_page_per_slide() {
        let deck = deck_with_text(&["Hello PDF", "Second slide"]);
        let export = export_pdf_bytes(&deck).expect("pdf builds");
        assert!(export.bytes.starts_with(b"%PDF-1.7"), "the header must be PDF 1.7");
        assert_eq!(count(&export.bytes, b"/Type /Page "), 2, "one page per slide");
        assert_eq!(count(&export.bytes, b"/MediaBox [0 0 960 540]"), 2, "the deck size is carried in points");
        assert!(count(&export.bytes, b"(Hello PDF)") == 1, "the text is painted");
        assert!(export.losses.is_empty(), "plain Latin text has no loss: {:?}", export.losses);
    }

    /// Smoke: WinAnsi cannot represent wide characters, so the run is reported.
    #[test]
    fn non_latin_text_is_reported_as_a_glyph_loss() {
        let deck = deck_with_text(&["caf\u{e9} \u{4e2d}\u{6587}"]);
        let export = export_pdf_bytes(&deck).expect("pdf builds");
        assert!(export.losses.iter().any(|loss| loss.code == LOSS_GLYPH), "glyph loss is reported");
    }

    /// Smoke: the backend refuses a deck with no pages instead of writing an
    /// empty page tree.
    #[test]
    fn an_empty_deck_is_refused() {
        let deck = Deck::create(DeckCreateOptions::default());
        let error = export_pdf_bytes(&deck).expect_err("a deck without slides has no page");
        assert_eq!(error.code, "vutex.render.pdf.no_pages");
    }
}
