//! Anti-aliased glyph rasterization built entirely on this crate's outlines.
//!
//! [`Font`] mirrors the small call surface consumers need — load bytes, ask
//! whether a character is covered, and rasterize it — while the outline
//! decoding (TrueType `glyf`, including composites, and OTF CFF) stays in the
//! table modules. Coordinates are scaled by `size / units_per_em`, the baseline
//! is `y = 0` with font-space `y` pointing up, and the returned bitmap is
//! row-major from the top edge down.
//!
//! Coverage is 8-bit (`0..=255`) and computed with a 4x4 sample grid under the
//! non-zero winding rule. All entry points are total: malformed or empty
//! outlines produce an empty bitmap with a still-correct advance.

use std::collections::HashMap;

use crate::{font::FontFace, outline::OutlineBuilder, sfnt::ParseError};

/// Rasterized glyph metrics, in pixels, relative to the pen position and
/// baseline.
///
/// `xmin`/`ymin` are the floored lower-left ink corner (font-space `y` up),
/// `width`/`height` are whole-pixel bitmap dimensions, and `advance_width` is
/// the horizontal advance in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    /// Left edge of the bitmap relative to the pen, in pixels.
    pub xmin: i32,
    /// Bottom edge of the bitmap relative to the baseline, in pixels.
    pub ymin: i32,
    /// Bitmap width in pixels.
    pub width: usize,
    /// Bitmap height in pixels.
    pub height: usize,
    /// Horizontal advance in pixels.
    pub advance_width: f32,
}

/// Errors produced when a font cannot be opened.
#[derive(Debug)]
pub enum FontError {
    /// The SFNT container could not be parsed.
    Parse(ParseError),
    /// The requested face index does not exist in the collection.
    NoFace(u32),
}

impl std::fmt::Display for FontError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(error) => write!(f, "{error}"),
            Self::NoFace(index) => write!(f, "font collection has no face {index}"),
        }
    }
}

impl std::error::Error for FontError {}

impl From<ParseError> for FontError {
    fn from(error: ParseError) -> Self {
        Self::Parse(error)
    }
}

/// A parsed face ready for glyph lookup and rasterization.
pub struct Font {
    face: FontFace,
    units_per_em: f32,
    num_glyphs: u16,
    cmap: HashMap<u32, u16>,
}

impl Font {
    /// Parse `bytes` and select one face from a single font or a TTC.
    ///
    /// `collection_index` is the TTC face index; pass `0` for standalone fonts.
    pub fn from_bytes(bytes: Vec<u8>, collection_index: u32) -> Result<Self, FontError> {
        let container = crate::sfnt::parse_font(&bytes)?;
        let face = container.face(collection_index as usize).ok_or(FontError::NoFace(collection_index))?.clone();
        let mut face = FontFace::from_face(face);
        let units_per_em = match face.units_per_em() {
            0 => 1000.0,
            value => f32::from(value),
        };
        let num_glyphs = face.maxp().map(|maxp| maxp.num_glyphs).unwrap_or(0);
        let cmap = face.cmap().map(|table| table.unified.clone()).unwrap_or_default();
        Ok(Self { face, units_per_em, num_glyphs, cmap })
    }

    /// Number of glyphs declared by `maxp`.
    #[inline]
    pub fn num_glyphs(&self) -> u16 {
        self.num_glyphs
    }

    /// True when `ch` maps to a real glyph (not `.notdef`).
    #[must_use]
    pub fn has_glyph(&self, ch: char) -> bool {
        self.cmap.get(&(ch as u32)).is_some_and(|glyph_id| *glyph_id != 0)
    }

    /// Rasterize `ch` at `size` pixels-per-em.
    ///
    /// A missing glyph, an empty outline or a malformed charstring yields a
    /// zero-sized bitmap; the advance is still reported so text layout stays
    /// stable.
    pub fn rasterize(&mut self, ch: char, size: f32) -> (Metrics, Vec<u8>) {
        let glyph_id = self.cmap.get(&(ch as u32)).copied();
        let scale = if size.is_finite() && size > 0.0 { size / self.units_per_em } else { 0.0 };
        // An unmapped character still advances by the `.notdef` width, matching
        // how a shaped line would treat a missing glyph.
        let metrics_glyph = glyph_id.unwrap_or(0);
        let advance = self.face.glyph_advance(metrics_glyph).map_or(0.0, |width| f32::from(width) * scale);
        let (metrics, bitmap) = match glyph_id {
            Some(glyph_id) if scale > 0.0 => {
                let mut builder = PathBuilder::new(scale);
                if self.face.outline_glyph(glyph_id, &mut builder).is_some() {
                    rasterize_contours(&builder.into_contours(), advance)
                }
                else {
                    (Metrics { xmin: 0, ymin: 0, width: 0, height: 0, advance_width: advance }, Vec::new())
                }
            }
            _ => (Metrics { xmin: 0, ymin: 0, width: 0, height: 0, advance_width: advance }, Vec::new()),
        };
        (metrics, bitmap)
    }
}

impl std::fmt::Debug for Font {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Font").field("units_per_em", &self.units_per_em).field("num_glyphs", &self.num_glyphs).finish()
    }
}

/// Flattening tolerance in pixels. Curves are subdivided until they deviate
/// from a straight chord by at most this much.
const FLATTEN_TOLERANCE: f32 = 0.1;
/// Safety cap on recursive curve subdivision.
const MAX_SUBDIVISION: u8 = 16;
/// Upper bound on bitmap area so an absurd size cannot exhaust memory.
const MAX_BITMAP_PIXELS: usize = 64 * 1024 * 1024;
/// Samples per pixel edge (4x4 = 16 coverage levels).
const SAMPLES_PER_AXIS: usize = 4;

type Point = [f32; 2];

/// Converts outline segments into flattened, closed polylines in pixel space.
struct PathBuilder {
    scale: f32,
    contours: Vec<Vec<Point>>,
    current: Vec<Point>,
    last: Point,
    start: Point,
}

impl PathBuilder {
    fn new(scale: f32) -> Self {
        Self { scale, contours: Vec::new(), current: Vec::new(), last: [0.0, 0.0], start: [0.0, 0.0] }
    }

    fn point(x: f32, y: f32, scale: f32) -> Point {
        [x * scale, y * scale]
    }

    fn flush(&mut self) {
        if self.current.len() >= 3 {
            self.contours.push(std::mem::take(&mut self.current));
        }
        else {
            self.current.clear();
        }
    }

    fn into_contours(mut self) -> Vec<Vec<Point>> {
        self.flush();
        self.contours
    }

    fn quadratic(&mut self, p0: Point, p1: Point, p2: Point, depth: u8) {
        if depth >= MAX_SUBDIVISION || distance_to_chord(p1, p0, p2) <= FLATTEN_TOLERANCE {
            self.current.push(p2);
            return;
        }
        let p01 = midpoint(p0, p1);
        let p12 = midpoint(p1, p2);
        let mid = midpoint(p01, p12);
        self.quadratic(p0, p01, mid, depth + 1);
        self.quadratic(mid, p12, p2, depth + 1);
    }

    fn cubic(&mut self, p0: Point, p1: Point, p2: Point, p3: Point, depth: u8) {
        let flatness = distance_to_chord(p1, p0, p3).max(distance_to_chord(p2, p0, p3));
        if depth >= MAX_SUBDIVISION || flatness <= FLATTEN_TOLERANCE {
            self.current.push(p3);
            return;
        }
        let p01 = midpoint(p0, p1);
        let p12 = midpoint(p1, p2);
        let p23 = midpoint(p2, p3);
        let p012 = midpoint(p01, p12);
        let p123 = midpoint(p12, p23);
        let mid = midpoint(p012, p123);
        self.cubic(p0, p01, p012, mid, depth + 1);
        self.cubic(mid, p123, p23, p3, depth + 1);
    }
}

impl OutlineBuilder for PathBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        self.flush();
        let point = Self::point(x, y, self.scale);
        self.current.push(point);
        self.start = point;
        self.last = point;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        if self.current.is_empty() {
            return;
        }
        let point = Self::point(x, y, self.scale);
        self.current.push(point);
        self.last = point;
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        if self.current.is_empty() {
            return;
        }
        let control = Self::point(x1, y1, self.scale);
        let end = Self::point(x, y, self.scale);
        let start = self.last;
        self.quadratic(start, control, end, 0);
        self.last = end;
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        if self.current.is_empty() {
            return;
        }
        let control1 = Self::point(x1, y1, self.scale);
        let control2 = Self::point(x2, y2, self.scale);
        let end = Self::point(x, y, self.scale);
        let start = self.last;
        self.cubic(start, control1, control2, end, 0);
        self.last = end;
    }

    fn close(&mut self) {
        if self.current.len() >= 3 {
            // Explicitly return to the contour start; the rasterizer also wraps.
            self.current.push(self.start);
            self.flush();
        }
        else {
            self.current.clear();
        }
    }
}

fn midpoint(a: Point, b: Point) -> Point {
    [(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5]
}

/// Perpendicular distance from `point` to the infinite line through `a`/`b`.
fn distance_to_chord(point: Point, a: Point, b: Point) -> f32 {
    let dx = b[0] - a[0];
    let dy = b[1] - a[1];
    let length = (dx * dx + dy * dy).sqrt();
    if length <= f32::EPSILON {
        return (point[0] - a[0]).hypot(point[1] - a[1]);
    }
    ((point[0] - a[0]) * dy - (point[1] - a[1]) * dx).abs() / length
}

/// Rasterize flattened contours into a top-down coverage bitmap.
fn rasterize_contours(contours: &[Vec<Point>], advance: f32) -> (Metrics, Vec<u8>) {
    let empty = Metrics { xmin: 0, ymin: 0, width: 0, height: 0, advance_width: advance };
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for point in contours.iter().flatten() {
        if !point[0].is_finite() || !point[1].is_finite() {
            continue;
        }
        min_x = min_x.min(point[0]);
        min_y = min_y.min(point[1]);
        max_x = max_x.max(point[0]);
        max_y = max_y.max(point[1]);
    }
    if min_x > max_x || min_y > max_y {
        return (empty, Vec::new());
    }

    let xmin = min_x.floor() as i32;
    let ymin = min_y.floor() as i32;
    let width = (max_x.ceil() as i32 - xmin).max(0) as usize;
    let height = (max_y.ceil() as i32 - ymin).max(0) as usize;
    if width == 0 || height == 0 {
        return (empty, Vec::new());
    }
    if width.saturating_mul(height) > MAX_BITMAP_PIXELS {
        return (empty, Vec::new());
    }

    let mut coverage = vec![0u8; width * height];
    let samples = (SAMPLES_PER_AXIS * SAMPLES_PER_AXIS) as u32;
    let top = (ymin + height as i32) as f32;
    for row in 0..height {
        let row_top = top - row as f32;
        for column in 0..width {
            let column_left = (xmin + column as i32) as f32;
            let mut covered = 0u32;
            for sample_y in 0..SAMPLES_PER_AXIS {
                let y = row_top - (sample_y as f32 + 0.5) / SAMPLES_PER_AXIS as f32;
                for sample_x in 0..SAMPLES_PER_AXIS {
                    let x = column_left + (sample_x as f32 + 0.5) / SAMPLES_PER_AXIS as f32;
                    if winding_number(contours, x, y) != 0 {
                        covered += 1;
                    }
                }
            }
            if covered > 0 {
                coverage[row * width + column] = ((covered * 255 + samples / 2) / samples) as u8;
            }
        }
    }

    (Metrics { xmin, ymin, width, height, advance_width: advance }, coverage)
}

/// Non-zero winding number of `point` against all contours.
fn winding_number(contours: &[Vec<Point>], x: f32, y: f32) -> i32 {
    let mut winding = 0i32;
    for contour in contours {
        let count = contour.len();
        if count < 3 {
            continue;
        }
        for index in 0..count {
            let a = contour[index];
            let b = contour[(index + 1) % count];
            if a[1] <= y {
                if b[1] > y && cross(a, b, x, y) > 0.0 {
                    winding += 1;
                }
            }
            else if b[1] <= y && cross(a, b, x, y) < 0.0 {
                winding -= 1;
            }
        }
    }
    winding
}

#[inline]
fn cross(a: Point, b: Point, x: f32, y: f32) -> f32 {
    (b[0] - a[0]) * (y - a[1]) - (x - a[0]) * (b[1] - a[1])
}
