//! High-level [`FontFace`] facade that aggregates parsed tables.
//!
//! This is the entry point consumers are expected to use for LatinModernMath-Regular.otf
//! and similar math fonts. The facade is intentionally non-magical — every sub-table
//! is parsed lazily on first access (or not at all if the table is missing).

use crate::{
    outline::{BBox, OutlineBuilder},
    reader::Reader,
    sfnt::Face,
    tables::{
        cff::CffTable,
        cmap::CmapTable,
        glyf::{Component, GlyphOutline, Point},
        head::HeadTable,
        hhea::HheaTable,
        hmtx::HmtxTable,
        loca::{Loca, LocaFormat, parse_loca},
        math::MathTable,
        maxp::MaxpTable,
        name::NameTable,
        os2::Os2Table,
        post::PostTable,
    },
};

/// Convenience: parse a [`FontFace`] from raw bytes.
pub fn load(data: &[u8]) -> Result<FontFace, crate::sfnt::ParseError> {
    let container = crate::sfnt::parse_font_validated(data)?;
    let face = container.first().ok_or(crate::sfnt::ParseError::TooSmall { need: 12, have: data.len() })?.clone();
    Ok(FontFace::from_face(face))
}

/// A single parsed font face with lazily-extracted tables.
pub struct FontFace {
    pub(crate) face: Face,
    pub(crate) name: Option<NameTable>,
    pub(crate) head: Option<HeadTable>,
    pub(crate) hhea: Option<HheaTable>,
    pub(crate) maxp: Option<MaxpTable>,
    pub(crate) os2: Option<Os2Table>,
    pub(crate) post: Option<PostTable>,
    pub(crate) hmtx: Option<HmtxTable>,
    pub(crate) cmap: Option<CmapTable>,
    pub(crate) math: Option<MathTable>,
    pub(crate) loca: Option<Loca>,
    pub(crate) cff: Option<CffTable>,
    pub(crate) cff_attempted: bool,
    pub(crate) is_opentype_cff: bool,
    pub(crate) is_truetype_glyf: bool,
}

impl std::fmt::Debug for FontFace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FontFace")
            .field("is_opentype_cff", &self.is_opentype_cff)
            .field("is_truetype_glyf", &self.is_truetype_glyf)
            .field("num_tables", &self.face.directory.len())
            .field("has_math", &self.math.is_some())
            .finish()
    }
}

impl FontFace {
    /// Convenience: parse a [`FontFace`] from raw bytes.
    pub fn load(data: &[u8]) -> Result<Self, crate::sfnt::ParseError> {
        let container = crate::sfnt::parse_font_validated(data)?;
        let face = container.first().ok_or(crate::sfnt::ParseError::TooSmall { need: 12, have: data.len() })?.clone();
        Ok(Self::from_face(face))
    }

    /// Wrap a pre-parsed [`Face`].
    pub fn from_face(face: Face) -> Self {
        let is_opentype_cff = face.header.is_opentype();
        let is_truetype_glyf = face.header.is_truetype();
        Self {
            face,
            name: None,
            head: None,
            hhea: None,
            maxp: None,
            os2: None,
            post: None,
            hmtx: None,
            cmap: None,
            math: None,
            loca: None,
            cff: None,
            cff_attempted: false,
            is_opentype_cff,
            is_truetype_glyf,
        }
    }

    /// Access the underlying [`Face`].
    #[inline]
    pub fn face(&self) -> &Face {
        &self.face
    }

    /// True if this face uses OTF + CFF outlines.
    #[inline]
    pub fn is_opentype_cff(&self) -> bool {
        self.is_opentype_cff
    }

    /// True if this face uses TrueType `glyf` outlines.
    #[inline]
    pub fn is_truetype_glyf(&self) -> bool {
        self.is_truetype_glyf
    }

    /// True if the font exposes a `MATH` table.
    #[inline]
    pub fn has_math(&self) -> bool {
        self.face.directory.get(b"MATH").is_some()
    }

    // --- standard table accessors ---

    pub fn name(&mut self) -> Option<&NameTable> {
        if self.name.is_none() {
            self.name = self.face.directory.get(b"name").and_then(|rec| {
                self.face.table_data(rec).ok().and_then(|data| {
                    let mut r = Reader::new(data);
                    NameTable::parse(&mut r).ok()
                })
            });
        }
        self.name.as_ref()
    }

    pub fn head(&mut self) -> Option<&HeadTable> {
        if self.head.is_none() {
            self.head = self.face.directory.get(b"head").and_then(|rec| {
                self.face.table_data(rec).ok().and_then(|data| {
                    let mut r = Reader::new(data);
                    HeadTable::parse(&mut r).ok()
                })
            });
        }
        self.head.as_ref()
    }

    pub fn hhea(&mut self) -> Option<&HheaTable> {
        if self.hhea.is_none() {
            self.hhea = self.face.directory.get(b"hhea").and_then(|rec| {
                self.face.table_data(rec).ok().and_then(|data| {
                    let mut r = Reader::new(data);
                    HheaTable::parse(&mut r).ok()
                })
            });
        }
        self.hhea.as_ref()
    }

    pub fn maxp(&mut self) -> Option<&MaxpTable> {
        if self.maxp.is_none() {
            self.maxp = self.face.directory.get(b"maxp").and_then(|rec| {
                self.face.table_data(rec).ok().and_then(|data| {
                    let mut r = Reader::new(data);
                    MaxpTable::parse(&mut r).ok()
                })
            });
        }
        self.maxp.as_ref()
    }

    pub fn os2(&mut self) -> Option<&Os2Table> {
        if self.os2.is_none() {
            self.os2 = self.face.directory.get(b"OS/2").and_then(|rec| {
                self.face.table_data(rec).ok().and_then(|data| {
                    let mut r = Reader::new(data);
                    Os2Table::parse(&mut r).ok()
                })
            });
        }
        self.os2.as_ref()
    }

    pub fn post(&mut self) -> Option<&PostTable> {
        if self.post.is_none() {
            self.post = self.face.directory.get(b"post").and_then(|rec| {
                self.face.table_data(rec).ok().and_then(|data| {
                    let mut r = Reader::new(data);
                    PostTable::parse(&mut r).ok()
                })
            });
        }
        self.post.as_ref()
    }

    pub fn cmap(&mut self) -> Option<&CmapTable> {
        if self.cmap.is_none() {
            self.cmap =
                self.face.directory.get(b"cmap").and_then(|rec| self.face.table_data(rec).ok().and_then(|data| CmapTable::parse(data).ok()));
        }
        self.cmap.as_ref()
    }

    pub fn hmtx(&mut self) -> Option<&HmtxTable> {
        if self.hmtx.is_none() {
            let num_hmetrics = self.hhea()?.num_hmetrics;
            let num_glyphs = self.maxp()?.num_glyphs;
            let face = &self.face;
            self.hmtx = face.directory.get(b"hmtx").and_then(|rec| {
                face.table_data(rec).ok().and_then(|data| {
                    let mut r = Reader::new(data);
                    HmtxTable::parse(&mut r, num_glyphs, num_hmetrics).ok()
                })
            });
        }
        self.hmtx.as_ref()
    }

    pub fn math(&mut self) -> Option<&MathTable> {
        if self.math.is_none() {
            let num_glyphs = self.maxp().map(|maxp| maxp.num_glyphs);
            self.math = self
                .face
                .directory
                .get(b"MATH")
                .and_then(|rec| self.face.table_data(rec).ok().and_then(|data| MathTable::parse_with_num_glyphs(data, num_glyphs).ok()));
        }
        self.math.as_ref()
    }

    /// TrueType outline loca table (only useful when `is_truetype_glyf()`).
    pub fn loca(&mut self) -> Option<&Loca> {
        if self.loca.is_none() {
            let num_glyphs = self.maxp()?.num_glyphs;
            let long_loca = self.head()?.uses_long_loca();
            let face = &self.face;
            let loca_rec = face.directory.get(b"loca")?;
            let data = face.table_data(loca_rec).ok()?;
            let mut r = Reader::new(data);
            let fmt = if long_loca { LocaFormat::Long } else { LocaFormat::Short };
            self.loca = parse_loca(&mut r, num_glyphs, fmt).ok();
        }
        self.loca.as_ref()
    }

    /// Iterate over TrueType glyph outlines (empty when this is an OTF CFF font).
    pub fn glyf_outlines(&mut self) -> Vec<GlyphOutline> {
        let Some(loca) = self.loca().cloned()
        else {
            return Vec::new();
        };
        let maxp = self.maxp().copied().unwrap_or_default();
        let Some(glyf_rec) = self.face.directory.get(b"glyf")
        else {
            return Vec::new();
        };
        let Ok(glyf_data) = self.face.table_data(glyf_rec)
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        crate::tables::glyf::iterate_glyf(glyf_data, &loca, maxp.num_glyphs, |_id, res| {
            if let Ok(g) = res {
                out.push(g);
            }
        });
        out
    }

    // ---- high-level glyph convenience API ----

    /// Shortcut for `self.head()?.units_per_em`.
    pub fn units_per_em(&mut self) -> u16 {
        self.head().map(|h| h.units_per_em).unwrap_or(1000)
    }

    /// Look up a Unicode scalar → glyph ID via `cmap`.
    pub fn glyph_index(&mut self, ch: char) -> Option<u16> {
        self.cmap()?.lookup(ch as u32)
    }

    /// Horizontal advance width for the given glyph.
    pub fn glyph_hor_advance(&mut self, glyph_id: u16) -> Option<u16> {
        self.hmtx()?.get(glyph_id).map(|m| m.advance_width)
    }

    /// Horizontal advance width honouring `USE_MY_METRICS` on composite glyphs.
    ///
    /// TrueType composites may borrow the metrics of one of their components;
    /// the flag is inherited recursively, so the lookup follows the chain up to
    /// a depth of eight components before falling back to the glyph's own
    /// `hmtx` entry.
    pub fn glyph_advance(&mut self, glyph_id: u16) -> Option<u16> {
        let direct = self.glyph_hor_advance(glyph_id);
        if self.is_truetype_glyf
            && let Some(source) = self.metrics_component(glyph_id, 0)
        {
            return self.glyph_hor_advance(source).or(direct);
        }
        direct
    }

    /// Find the component that supplies the metrics of `glyph_id`.
    fn metrics_component(&mut self, glyph_id: u16, depth: u8) -> Option<u16> {
        if depth > MAX_COMPOSITE_DEPTH {
            return None;
        }
        let outline = self.parsed_glyf(glyph_id)?;
        if outline.num_contours >= 0 {
            return None;
        }
        if let Some(component) = outline.components.iter().find(|component| component.use_my_metrics) {
            let nested = self.metrics_component(component.glyph_id, depth + 1);
            return nested.or(Some(component.glyph_id));
        }
        // Some fonts only set the flag on a nested component.
        outline.components.iter().find_map(|component| self.metrics_component(component.glyph_id, depth + 1))
    }

    /// Parse one glyph from `glyf`/`loca` without expanding components.
    fn parsed_glyf(&mut self, glyph_id: u16) -> Option<GlyphOutline> {
        let num_glyphs = self.maxp()?.num_glyphs;
        if glyph_id >= num_glyphs {
            return None;
        }
        let loca = self.loca().cloned()?;
        let record = self.face.directory.get(b"glyf")?;
        let data = self.face.table_data(record).ok()?;
        let offset = loca.glyph_offset(glyph_id) as usize;
        let size = loca.glyph_size(glyph_id) as usize;
        let end = offset.checked_add(size)?;
        if size == 0 || end > data.len() {
            return None;
        }
        crate::tables::glyf::parse_glyph(glyph_id, &data[offset..end]).ok()
    }

    /// Per-glyph bounding box in font units.
    ///
    /// Composite TrueType glyphs use the embedded header box when it is
    /// populated; a degenerate box is recomputed from the expanded outline.
    /// OTF CFF fonts derive the box from the Type 2 charstring.
    pub fn glyph_bounding_box(&mut self, glyph_id: u16) -> Option<BBox> {
        if self.is_truetype_glyf {
            let outline = self.parsed_glyf(glyph_id)?;
            if outline.num_contours == 0 {
                return None;
            }
            if outline.num_contours > 0 {
                return Some(BBox { x_min: outline.x_min, y_min: outline.y_min, x_max: outline.x_max, y_max: outline.y_max });
            }
            if outline.x_min != 0 || outline.y_min != 0 || outline.x_max != 0 || outline.y_max != 0 {
                return Some(BBox { x_min: outline.x_min, y_min: outline.y_min, x_max: outline.x_max, y_max: outline.y_max });
            }
            let num_glyphs = self.maxp()?.num_glyphs;
            let loca = self.loca().cloned()?;
            let record = self.face.directory.get(b"glyf")?;
            let data = self.face.table_data(record).ok()?;
            let contours = expand_glyf(data, &loca, num_glyphs, glyph_id, 0)?;
            bbox_of_contours(&contours)
        }
        else {
            self.cff_glyph_bounding_box(glyph_id)
        }
    }

    /// Emit glyph outline segments into the given [`OutlineBuilder`].
    ///
    /// Returns `Some(())` when an outline was successfully decoded. `None`
    /// when the glyph has no outline (e.g. empty space glyph) or the
    /// outline kind is not available.
    pub fn outline_glyph(&mut self, glyph_id: u16, builder: &mut dyn OutlineBuilder) -> Option<()> {
        if self.is_truetype_glyf { self.outline_glyf(glyph_id, builder) } else { self.outline_cff(glyph_id, builder) }
    }

    // ---- TrueType glyf → OutlineBuilder ----

    fn outline_glyf(&mut self, glyph_id: u16, builder: &mut dyn OutlineBuilder) -> Option<()> {
        let num_glyphs = self.maxp()?.num_glyphs;
        if glyph_id >= num_glyphs {
            return None;
        }
        let loca = self.loca().cloned()?;
        let record = self.face.directory.get(b"glyf")?;
        let data = self.face.table_data(record).ok()?;
        let contours = expand_glyf(data, &loca, num_glyphs, glyph_id, 0)?;
        if contours.is_empty() {
            return None;
        }
        for contour in &contours {
            emit_contour(contour, builder);
        }
        Some(())
    }

    // ---- OTF CFF → OutlineBuilder ----

    /// Lazily parse and cache the `CFF ` table.
    fn cff_table(&mut self) -> Option<&CffTable> {
        if !self.cff_attempted {
            self.cff_attempted = true;
            let record = self.face.directory.get(b"CFF ")?;
            let data = self.face.table_data(record).ok()?;
            self.cff = CffTable::parse(data).ok();
        }
        self.cff.as_ref()
    }

    fn outline_cff(&mut self, glyph_id: u16, builder: &mut dyn OutlineBuilder) -> Option<()> {
        self.cff_table()?.outline_glyph(glyph_id, builder)
    }

    fn cff_glyph_bounding_box(&mut self, glyph_id: u16) -> Option<BBox> {
        self.cff_table()?.glyph_bbox(glyph_id)
    }

    // --- convenience strings ---

    pub fn family(&mut self) -> Option<String> {
        self.name()?
            .get(crate::tables::name::NameId::FAMILY)
            .map(|s| s.to_string())
            .or_else(|| self.name()?.get(crate::tables::name::NameId::TYPOGRAPHIC_FAMILY).map(|s| s.to_string()))
    }

    pub fn subfamily(&mut self) -> Option<String> {
        self.name()?.get(crate::tables::name::NameId::SUBFAMILY).map(|s| s.to_string())
    }

    pub fn full_name(&mut self) -> Option<String> {
        self.name()?.get(crate::tables::name::NameId::FULL).map(|s| s.to_string())
    }

    pub fn postscript_name(&mut self) -> Option<String> {
        self.name()?.get(crate::tables::name::NameId::POSTSCRIPT).map(|s| s.to_string())
    }

    pub fn license(&mut self) -> Option<String> {
        self.name()?.get(crate::tables::name::NameId::LICENSE).map(|s| s.to_string())
    }

    pub fn license_url(&mut self) -> Option<String> {
        self.name()?.get(crate::tables::name::NameId::LICENSE_URL).map(|s| s.to_string())
    }
}

// Provide a way to look up arbitrary tags too (advanced access).
impl FontFace {
    pub fn table_bytes(&self, tag_bytes: &[u8; 4]) -> Option<&[u8]> {
        let rec = self.face.directory.get(tag_bytes)?;
        self.face.table_data(rec).ok()
    }

    pub fn table_record(&self, tag_bytes: &[u8; 4]) -> Option<&crate::sfnt::TableRecord> {
        self.face.directory.get(tag_bytes)
    }
}

// ---- TrueType glyph expansion ----

/// Maximum composite nesting depth. Deeper (or self-referencing) glyphs are
/// treated as empty instead of recursing without bound.
const MAX_COMPOSITE_DEPTH: u8 = 8;

/// Expand a TrueType glyph into absolute contours, resolving composites.
///
/// Each component's 2x2 transform and either its XY translation or its anchor
/// point pair is applied recursively. A missing or malformed component
/// contributes no contours, and nesting stops at [`MAX_COMPOSITE_DEPTH`].
fn expand_glyf(glyf_data: &[u8], loca: &Loca, num_glyphs: u16, glyph_id: u16, depth: u8) -> Option<Vec<Vec<Point>>> {
    if depth > MAX_COMPOSITE_DEPTH || glyph_id >= num_glyphs {
        return Some(Vec::new());
    }
    let offset = loca.glyph_offset(glyph_id) as usize;
    let size = loca.glyph_size(glyph_id) as usize;
    let end = offset.checked_add(size)?;
    if size == 0 || end > glyf_data.len() {
        return Some(Vec::new());
    }
    let outline = match crate::tables::glyf::parse_glyph(glyph_id, &glyf_data[offset..end]) {
        Ok(outline) => outline,
        Err(_) => return Some(Vec::new()),
    };
    if !outline.is_composite {
        return Some(split_contours(&outline));
    }

    let mut contours: Vec<Vec<Point>> = Vec::new();
    let mut assembled: Vec<Point> = Vec::new();
    for component in &outline.components {
        let Some(mut child) = expand_glyf(glyf_data, loca, num_glyphs, component.glyph_id, depth + 1)
        else {
            continue;
        };
        apply_matrix(&mut child, component);
        let child_points: Vec<Point> = child.iter().flatten().copied().collect();
        let (dx, dy) = component_translation(component, &assembled, &child_points);
        for contour in &mut child {
            for point in contour.iter_mut() {
                point.x += dx;
                point.y += dy;
            }
            assembled.extend(contour.iter().copied());
            contours.push(std::mem::take(contour));
        }
    }
    Some(contours)
}

/// Split a simple glyph's point ring into per-contour slices.
fn split_contours(outline: &GlyphOutline) -> Vec<Vec<Point>> {
    if outline.num_contours <= 0 || outline.points.is_empty() {
        return Vec::new();
    }
    let mut contours = Vec::with_capacity(outline.num_contours as usize);
    for index in 0..outline.num_contours as usize {
        let start = outline.contours[index];
        let end = outline.contours[index + 1].min(outline.points.len());
        if end > start {
            contours.push(outline.points[start..end].to_vec());
        }
    }
    contours
}

/// Apply a component's 2x2 matrix to every point of its child contours.
fn apply_matrix(contours: &mut [Vec<Point>], component: &Component) {
    let [a, b, c, d] = component.matrix();
    for contour in contours {
        for point in contour.iter_mut() {
            let x = point.x as f32;
            let y = point.y as f32;
            point.x = (a * x + c * y).round() as i32;
            point.y = (b * x + d * y).round() as i32;
        }
    }
}

/// Resolve the translation of one component from its XY arguments or anchors.
fn component_translation(component: &Component, parent: &[Point], child: &[Point]) -> (i32, i32) {
    if component.args_are_xy_values {
        let (mut dx, mut dy) = (component.x_offset, component.y_offset);
        if component.scaled_offset() {
            let [a, b, c, d] = component.matrix();
            let (x, y) = (dx as f32, dy as f32);
            dx = (a * x + c * y).round() as i32;
            dy = (b * x + d * y).round() as i32;
        }
        return (dx, dy);
    }
    let parent_point = parent.get(component.arg1.max(0) as usize).copied();
    let child_point = child.get(component.arg2.max(0) as usize).copied();
    match (parent_point, child_point) {
        (Some(parent_point), Some(child_point)) => (parent_point.x - child_point.x, parent_point.y - child_point.y),
        _ => (0, 0),
    }
}

/// Bounding box of expanded contours, clamped to the SFNT `i16` range.
fn bbox_of_contours(contours: &[Vec<Point>]) -> Option<BBox> {
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    for point in contours.iter().flatten() {
        min_x = min_x.min(point.x);
        min_y = min_y.min(point.y);
        max_x = max_x.max(point.x);
        max_y = max_y.max(point.y);
    }
    if min_x > max_x {
        return None;
    }
    let clamp = |value: i32| value.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    Some(BBox { x_min: clamp(min_x), y_min: clamp(min_y), x_max: clamp(max_x), y_max: clamp(max_y) })
}

/// Emit one closed TrueType contour as line and quadratic segments.
///
/// TrueType stores contours as a ring of points. Two consecutive off-curve
/// points imply an on-curve midpoint between them; a ring with no on-curve
/// point at all starts at the midpoint of its last and first points.
fn emit_contour(points: &[Point], builder: &mut dyn OutlineBuilder) {
    if points.len() < 2 {
        return;
    }
    let count = points.len();
    let (start, start_index) = match points.iter().position(|point| point.on_curve) {
        Some(index) => (points[index], index),
        None => {
            let last = points[count - 1];
            let first = points[0];
            (Point { x: (last.x + first.x) / 2, y: (last.y + first.y) / 2, on_curve: true }, 0)
        }
    };
    builder.move_to(start.x as f32, start.y as f32);
    let mut control: Option<Point> = None;
    for step in 0..count {
        let index = (start_index + step) % count;
        let point = points[index];
        if point.on_curve {
            if let Some(control) = control.take() {
                builder.quad_to(control.x as f32, control.y as f32, point.x as f32, point.y as f32);
            }
            else if index != start_index {
                builder.line_to(point.x as f32, point.y as f32);
            }
        }
        else if let Some(current) = control {
            let mid = Point { x: (current.x + point.x) / 2, y: (current.y + point.y) / 2, on_curve: true };
            builder.quad_to(current.x as f32, current.y as f32, mid.x as f32, mid.y as f32);
            control = Some(point);
        }
        else {
            control = Some(point);
        }
    }
    if let Some(control) = control {
        builder.quad_to(control.x as f32, control.y as f32, start.x as f32, start.y as f32);
    }
    builder.close();
}
