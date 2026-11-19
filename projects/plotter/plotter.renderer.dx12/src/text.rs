//! CPU glyph rasterisation composited over the Direct3D 12 frame.
//!
//! Text has no triangle conversion: the device draws geometry, then glyph
//! coverage bitmaps are blended into the readback image in scene order. Faces
//! are system files, so a missing font or an uncovered character is a
//! structured [`RenderError`], never a silent blank. Glyph bitmaps and the
//! parsed faces are cached for the life of the process.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

use data_binary_ttf::raster::Font;
use plotter_types::plot::Rgba;

use crate::{error::RenderError, geometry::TextRun};

/// Rasterised glyph plus its placement in pixels.
#[derive(Clone)]
struct Glyph {
    /// Horizontal advance in pixels.
    advance: f32,
    /// Horizontal offset from the pen to the bitmap's left edge.
    xmin: f32,
    /// Distance from the baseline up to the bitmap's top edge.
    top: f32,
    /// Bitmap width in pixels.
    width: usize,
    /// Bitmap height in pixels.
    height: usize,
    /// Row-major coverage, `width * height` bytes.
    coverage: Vec<u8>,
}

/// Parsed font faces plus the per-size glyph cache.
struct Raster {
    faces: Vec<Font>,
    cache: HashMap<(char, u32), Glyph>,
}

impl Raster {
    /// Opens the first usable system face. All probed paths are reported on failure.
    fn load() -> Result<Self, RenderError> {
        let mut tried = Vec::new();
        let mut faces = Vec::new();
        for path in font_candidates() {
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(_) => {
                    tried.push(path);
                    continue;
                }
            };
            match Font::from_bytes(bytes, 0) {
                Ok(face) => faces.push(face),
                Err(_) => tried.push(path),
            }
        }
        if faces.is_empty() {
            return Err(RenderError::FontUnavailable { tried });
        }
        Ok(Self { faces, cache: HashMap::new() })
    }

    /// Rasterises `ch` at `size`, caching by character and quarter-pixel size.
    ///
    /// The raster size is the quantised cache key so two near-identical sizes can
    /// never share an entry while being rasterised at different scales.
    fn glyph(&mut self, ch: char, size: f32) -> Result<Glyph, RenderError> {
        let quarter_pixels = ((size.max(1.0)) * 4.0).round() as u32;
        let key = (ch, quarter_pixels);
        if let Some(hit) = self.cache.get(&key) {
            return Ok(hit.clone());
        }
        let index = self.faces.iter().position(|face| face.has_glyph(ch)).ok_or(RenderError::MissingGlyph { character: ch })?;
        let (metrics, coverage) = self.faces[index].rasterize(ch, quarter_pixels as f32 / 4.0);
        let glyph = Glyph {
            advance: metrics.advance_width,
            xmin: metrics.xmin as f32,
            top: (metrics.ymin + metrics.height as i32) as f32,
            width: metrics.width,
            height: metrics.height,
            coverage,
        };
        self.cache.insert(key, glyph.clone());
        Ok(glyph)
    }
}

/// Process-wide parsed faces and glyph cache.
fn raster_slot() -> &'static Mutex<Option<Raster>> {
    static RASTER: OnceLock<Mutex<Option<Raster>>> = OnceLock::new();
    RASTER.get_or_init(|| Mutex::new(None))
}

/// Candidate system faces, most preferred first.
fn font_candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(custom) = std::env::var_os("PLOTTER_FONT_PATH") {
        paths.push(PathBuf::from(custom));
    }
    #[cfg(windows)]
    {
        let root = std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        let fonts = root.join("Fonts");
        paths.push(fonts.join("segoeui.ttf"));
        paths.push(fonts.join("msyh.ttc"));
    }
    #[cfg(target_os = "macos")]
    {
        paths.push(PathBuf::from("/System/Library/Fonts/Supplemental/Arial.ttf"));
        paths.push(PathBuf::from("/System/Library/Fonts/PingFang.ttc"));
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        paths.push(PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"));
        paths.push(PathBuf::from("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"));
    }
    paths
}

/// Composites every text run onto `pixels`, in order.
pub(crate) fn draw(pixels: &mut [u8], width: u32, height: u32, runs: &[TextRun]) -> Result<(), RenderError> {
    if runs.is_empty() {
        return Ok(());
    }
    let mut slot = raster_slot().lock().map_err(|_| RenderError::Backend("text raster lock is poisoned".into()))?;
    if slot.is_none() {
        *slot = Some(Raster::load()?);
    }
    let raster = slot.as_mut().expect("the raster slot was just filled");
    for run in runs {
        draw_run(raster, pixels, width, height, run)?;
    }
    Ok(())
}

fn draw_run(raster: &mut Raster, pixels: &mut [u8], width: u32, height: u32, run: &TextRun) -> Result<(), RenderError> {
    let left = run.position.x as f32;
    let mut pen = left;
    let mut baseline = height as f32 - run.position.y as f32;
    let line_advance = run.size.max(1.0) * 1.25;
    for ch in run.content.chars() {
        if ch == '\n' {
            pen = left;
            baseline -= line_advance;
            continue;
        }
        if ch == '\r' {
            continue;
        }
        let glyph = raster.glyph(ch, run.size)?;
        if glyph.width > 0 && glyph.height > 0 {
            let origin_x = pen.round() as i32 + glyph.xmin.round() as i32;
            let origin_y = baseline.round() as i32 - glyph.top.round() as i32;
            blend_glyph(pixels, width, height, &glyph, origin_x, origin_y, run.color);
        }
        pen += glyph.advance;
    }
    Ok(())
}

/// Source-over blend of one coverage bitmap at `origin` (top-left, device space).
fn blend_glyph(pixels: &mut [u8], width: u32, height: u32, glyph: &Glyph, origin_x: i32, origin_y: i32, color: Rgba) {
    let color_alpha = color.a.clamp(0.0, 1.0);
    if color_alpha <= 0.0 {
        return;
    }
    for row in 0..glyph.height {
        let y = origin_y + row as i32;
        if y < 0 || y >= height as i32 {
            continue;
        }
        for col in 0..glyph.width {
            let x = origin_x + col as i32;
            if x < 0 || x >= width as i32 {
                continue;
            }
            let coverage = u32::from(glyph.coverage[row * glyph.width + col]);
            if coverage == 0 {
                continue;
            }
            let source_alpha = coverage as f32 / 255.0 * color_alpha;
            if source_alpha <= 0.0 {
                continue;
            }
            let index = ((y as u32 * width + x as u32) as usize) * 4;
            let destination_alpha = f32::from(pixels[index + 3]) / 255.0;
            let out_alpha = source_alpha + destination_alpha * (1.0 - source_alpha);
            for (channel, value) in [color.r, color.g, color.b].into_iter().enumerate() {
                let source = value.clamp(0.0, 1.0);
                let destination = f32::from(pixels[index + channel]) / 255.0;
                let blended = (source * source_alpha + destination * destination_alpha * (1.0 - source_alpha)) / out_alpha;
                pixels[index + channel] = (blended * 255.0).round().clamp(0.0, 255.0) as u8;
            }
            pixels[index + 3] = (out_alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        }
    }
}
