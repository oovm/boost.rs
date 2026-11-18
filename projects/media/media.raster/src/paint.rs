//! Software rasterizer: paint operations on the `Frame` storage owned by
//! `media.image`. Everything here is plain integer/floating math; there is
//! no GPU and no external imaging library.

use geometry::Rgba;
use media_image::{Bitmap, Fit, Frame};
use media_types::hash;

/// Paint semantics for an RGBA8 frame: blending, primitives, image placement
/// and camera resampling. The pixel storage itself lives in `media.image`.
pub trait Paint {
    /// Source-over blend of one pixel, honoring the active clip.
    fn blend(&mut self, x: u32, y: u32, c: Rgba);

    /// Additive blend of one pixel (light, glow, overlays).
    fn add_pixel(&mut self, x: u32, y: u32, c: Rgba);

    /// Paints a solid color everywhere.
    fn fill(&mut self, c: Rgba);

    /// Paints a vertical gradient through the given color stops (evenly spaced).
    fn v_gradient(&mut self, stops: &[Rgba]);

    /// Paints a radial gradient from `center` color to `edge` color.
    fn radial(&mut self, center: Rgba, edge: Rgba);

    /// Source-over rectangle. Coordinates are clamped to the frame.
    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Rgba);

    /// Rounded rectangle with a corner radius in pixels.
    fn round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, c: Rgba);

    /// Camera transform: samples `src` around `focus` at `zoom` and optional
    /// rotation, then writes the result into this frame. Used to push, pull and
    /// pan a whole scene like a real lens.
    fn resample_from(&mut self, src: &Frame, zoom: f32, focus: [f32; 2], rotate_deg: f32);

    /// Paints a soft-edged band across the frame at `angle` degrees.
    ///
    /// Falloff is gaussian so the band reads as light in the air rather than as
    /// a painted stripe.
    fn band(&mut self, center_y: f32, angle_deg: f32, thickness: f32, c: Rgba);

    /// Sprinkles deterministic grain; `seed` changes per frame.
    fn grain(&mut self, density: u32, strength: f32, seed: u32);

    /// Fills a closed polygon with 4x vertical supersampling for soft edges.
    ///
    /// Coverage is accumulated per pixel and composited once, so a solid fill
    /// stays solid instead of stacking four translucent passes.
    fn fill_polygon(&mut self, points: &[[f32; 2]], c: Rgba, additive: bool);

    /// Strokes a polyline, optionally only the first `progress` of its length.
    fn stroke_polyline(&mut self, points: &[[f32; 2]], width: f32, c: Rgba, progress: f32, additive: bool);

    /// Ring or disc.
    fn circle(&mut self, cx: f32, cy: f32, radius: f32, c: Rgba, stroke: Option<f32>);

    /// Ellipse or elliptical ring with an `rx` × `ry` box.
    ///
    /// The box is the primitive, not the radius: a scene shape declares `w` and
    /// `h`, and both must reach the silhouette. `circle` is the `rx == ry` case.
    fn ellipse(&mut self, cx: f32, cy: f32, rx: f32, ry: f32, c: Rgba, stroke: Option<f32>);

    /// Straight line with thickness.
    fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, thickness: f32, c: Rgba);

    /// Additive soft glow dot for star fields and highlights.
    fn glow(&mut self, cx: f32, cy: f32, radius: f32, c: Rgba);

    /// Draws `bitmap` into the box `(x, y, w, h)` honoring `fit`.
    ///
    /// `alpha` scales the source alpha; `radius` clips the result to rounded
    /// corners (0 draws a plain rectangle).
    fn draw_bitmap(&mut self, bitmap: &Bitmap, x: f32, y: f32, w: f32, h: f32, fit: Fit, alpha: f32, radius: f32);

    /// Deterministic star field: hash-based positions, sine twinkle, slow drift.
    fn starfield(&mut self, count: u32, tint: Rgba, drift_px_per_sec: f32, twinkle: f32, seconds: f32);
}

#[inline]
fn inside_clip(frame: &Frame, x: u32, y: u32) -> bool {
    match frame.clip() {
        None => true,
        Some([x0, y0, x1, y1]) => x >= x0 && x < x1 && y >= y0 && y < y1,
    }
}

#[inline]
fn set_pixel(frame: &mut Frame, x: u32, y: u32, c: Rgba) {
    let i = ((y * frame.width + x) as usize) * 4;
    frame.pixels[i] = c.0[0];
    frame.pixels[i + 1] = c.0[1];
    frame.pixels[i + 2] = c.0[2];
    frame.pixels[i + 3] = c.0[3];
}

#[inline]
fn add(frame: &mut Frame, x: u32, y: u32, c: Rgba) {
    let i = ((y * frame.width + x) as usize) * 4;
    for k in 0..3 {
        let v = frame.pixels[i + k] as u32 + (c.0[k] as u32 * c.0[3] as u32 / 255);
        frame.pixels[i + k] = v.min(255) as u8;
    }
}

fn add_pixel_line(frame: &mut Frame, x0: f32, y0: f32, x1: f32, y1: f32, c: Rgba) {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let steps = dx.abs().max(dy.abs()).max(1.0) as u32;
    for s in 0..=steps {
        let t = s as f32 / steps as f32;
        let px = x0 + dx * t;
        let py = y0 + dy * t;
        if px < 0.0 || py < 0.0 || px >= frame.width as f32 || py >= frame.height as f32 {
            continue;
        }
        frame.add_pixel(px as u32, py as u32, c);
    }
}

fn stamp_disc(frame: &mut Frame, cx: f32, cy: f32, radius: f32, c: Rgba, additive: bool) {
    let pad = radius + 1.0;
    let x0 = (cx - pad).max(0.0) as u32;
    let y0 = (cy - pad).max(0.0) as u32;
    let x1 = (cx + pad).min(frame.width as f32 - 1.0) as u32;
    let y1 = (cy + pad).min(frame.height as f32 - 1.0) as u32;
    for py in y0..=y1 {
        for px in x0..=x1 {
            let dx = px as f32 - cx;
            let dy = py as f32 - cy;
            let d = (dx * dx + dy * dy).sqrt();
            blend_cover(frame, px, py, c, radius + 0.5 - d, additive);
        }
    }
}

/// Foundations of a soft edge: the coverage a pixel receives from a shape whose
/// signed distance is `cover`. One full pixel of falloff is enough to remove the
/// staircase from lines, rings and rounded corners without blurring the fill.
#[inline]
fn blend_cover(frame: &mut Frame, x: u32, y: u32, c: Rgba, cover: f32, additive: bool) {
    if cover <= 0.002 {
        return;
    }
    let alpha = (c.0[3] as f32 * cover.min(1.0) + 0.5) as u32;
    if alpha == 0 {
        return;
    }
    let color = Rgba([c.0[0], c.0[1], c.0[2], alpha.min(255) as u8]);
    if additive {
        frame.add_pixel(x, y, color);
    }
    else {
        frame.blend(x, y, color);
    }
}

/// Signed pixel distance from an ellipse boundary (positive inside), taken to
/// first order along the outward normal. The gradient correction is what keeps
/// the one-pixel feather even around a stretched silhouette instead of thinning
/// on the wide axis; for `rx == ry` it collapses to the plain `r - d` distance.
#[inline]
fn ellipse_cover(dx: f32, dy: f32, rx: f32, ry: f32) -> f32 {
    let gx = dx / (rx * rx);
    let gy = dy / (ry * ry);
    let g = (dx * gx + dy * gy).sqrt();
    if g <= 1e-6 {
        return rx.min(ry);
    }
    let grad = (gx * gx + gy * gy).sqrt();
    if grad <= 1e-6 {
        return rx.min(ry);
    }
    (1.0 - g) * g / grad
}

/// Rasterises one segment by sampling its distance field, so a pixel the stroke
/// only clips gets partial coverage instead of a hard on/off decision. Compositing
/// once per pixel also stops a semi-transparent line from stacking into a darker
/// one wherever the old disc stamps overlapped.
fn stroke_segment(frame: &mut Frame, a: [f32; 2], b: [f32; 2], half: f32, c: Rgba, additive: bool) {
    let pad = half + 1.0;
    let x0 = (a[0].min(b[0]) - pad).max(0.0) as u32;
    let y0 = (a[1].min(b[1]) - pad).max(0.0) as u32;
    let x1 = (a[0].max(b[0]) + pad).min(frame.width as f32 - 1.0) as u32;
    let y1 = (a[1].max(b[1]) + pad).min(frame.height as f32 - 1.0) as u32;
    let dx = b[0] - a[0];
    let dy = b[1] - a[1];
    let len_sq = (dx * dx + dy * dy).max(1e-6);
    for py in y0..=y1 {
        for px in x0..=x1 {
            let fx = px as f32;
            let fy = py as f32;
            let t = (((fx - a[0]) * dx + (fy - a[1]) * dy) / len_sq).clamp(0.0, 1.0);
            let nx = a[0] + dx * t;
            let ny = a[1] + dy * t;
            let d = ((fx - nx) * (fx - nx) + (fy - ny) * (fy - ny)).sqrt();
            blend_cover(frame, px, py, c, half + 0.5 - d, additive);
        }
    }
}

impl Paint for Frame {
    #[inline]
    fn blend(&mut self, x: u32, y: u32, c: Rgba) {
        if !inside_clip(self, x, y) {
            return;
        }
        let i = ((y * self.width + x) as usize) * 4;
        let a = c.0[3] as u32;
        if a >= 255 {
            set_pixel(self, x, y, c);
            return;
        }
        if a == 0 {
            return;
        }
        let inv = 255 - a;
        for k in 0..4 {
            let dst = self.pixels[i + k] as u32;
            self.pixels[i + k] = ((c.0[k] as u32 * a + dst * inv) / 255) as u8;
        }
    }

    #[inline]
    fn add_pixel(&mut self, x: u32, y: u32, c: Rgba) {
        if !inside_clip(self, x, y) {
            return;
        }
        add(self, x, y, c);
    }

    fn fill(&mut self, c: Rgba) {
        for chunk in self.pixels.chunks_mut(4) {
            chunk.copy_from_slice(&c.0);
        }
    }

    fn v_gradient(&mut self, stops: &[Rgba]) {
        if stops.is_empty() {
            return;
        }
        if stops.len() == 1 {
            self.fill(stops[0]);
            return;
        }
        let h = self.height;
        let w = self.width;
        let segments = stops.len() - 1;
        for y in 0..h {
            let t = (y as f32 / (h - 1).max(1) as f32) * segments as f32;
            let seg = (t as usize).min(segments - 1);
            let local = t - seg as f32;
            let c = stops[seg].lerp(stops[seg + 1], local);
            let row = (y as usize) * (w as usize) * 4;
            for px in self.pixels[row..row + (w as usize) * 4].chunks_mut(4) {
                px.copy_from_slice(&c.0);
            }
        }
    }

    fn radial(&mut self, center: Rgba, edge: Rgba) {
        let cx = self.width as f32 / 2.0;
        let cy = self.height as f32 / 2.0;
        let max_r = (cx * cx + cy * cy).sqrt();
        for y in 0..self.height {
            for x in 0..self.width {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                let t = (dx * dx + dy * dy).sqrt() / max_r;
                let c = center.lerp(edge, t);
                set_pixel(self, x, y, c);
            }
        }
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Rgba) {
        let x0 = x.max(0.0) as u32;
        let y0 = y.max(0.0) as u32;
        let x1 = (x + w).min(self.width as f32) as u32;
        let y1 = (y + h).min(self.height as f32) as u32;
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        for py in y0..y1 {
            for px in x0..x1 {
                self.blend(px, py, c);
            }
        }
    }

    fn round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, c: Rgba) {
        if radius <= 0.01 {
            self.rect(x, y, w, h, c);
            return;
        }
        let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
        let x0 = (x - 1.0).max(0.0) as u32;
        let y0 = (y - 1.0).max(0.0) as u32;
        let x1 = (x + w + 1.0).min(self.width as f32 - 1.0) as u32;
        let y1 = (y + h + 1.0).min(self.height as f32 - 1.0) as u32;
        let cx = x + w / 2.0;
        let cy = y + h / 2.0;
        let half_w = w / 2.0;
        let half_h = h / 2.0;
        for py in y0..=y1 {
            for px in x0..=x1 {
                // Signed distance to the rounded box: negative inside, so a
                // pixel straddling the border gets partial coverage.
                let qx = (px as f32 - cx).abs() - (half_w - r);
                let qy = (py as f32 - cy).abs() - (half_h - r);
                let outside = (qx.max(0.0) * qx.max(0.0) + qy.max(0.0) * qy.max(0.0)).sqrt() + qx.max(qy).min(0.0);
                blend_cover(self, px, py, c, 0.5 - (outside - r), false);
            }
        }
    }

    fn resample_from(&mut self, src: &Frame, zoom: f32, focus: [f32; 2], rotate_deg: f32) {
        let zoom = zoom.max(0.05);
        let angle = rotate_deg.to_radians();
        let (sin, cos) = angle.sin_cos();
        let w = self.width as f32;
        let h = self.height as f32;
        // How many source texels land on one output pixel. Above 1 the frame is
        // shrinking, so a single bilinear tap would shimmer and alias; average a
        // small box instead.
        let ratio = src.width as f32 / (w * zoom);
        let taps = (ratio.ceil() as u32).clamp(1, 3);
        let step = ratio / taps as f32;
        for y in 0..self.height {
            let v = (y as f32 + 0.5) / h - 0.5;
            for x in 0..self.width {
                let u = (x as f32 + 0.5) / w - 0.5;
                // Undo zoom, then rotate the sample position.
                let zu = u / zoom;
                let zv = v / zoom;
                let ru = zu * cos + zv * sin;
                let rv = -zu * sin + zv * cos;
                let sx = (focus[0] + ru) * src.width as f32 - 0.5;
                let sy = (focus[1] + rv) * src.height as f32 - 0.5;
                let texel = if taps == 1 {
                    src.sample(sx, sy)
                }
                else {
                    let mut acc = [0.0f32; 4];
                    let half = (taps as f32 - 1.0) / 2.0;
                    for ty in 0..taps {
                        for tx in 0..taps {
                            let t = src.sample(sx + (tx as f32 - half) * step, sy + (ty as f32 - half) * step);
                            for k in 0..4 {
                                acc[k] += t[k];
                            }
                        }
                    }
                    let n = (taps * taps) as f32;
                    [acc[0] / n, acc[1] / n, acc[2] / n, acc[3] / n]
                };
                let i = ((y * self.width + x) as usize) * 4;
                self.pixels[i] = texel[0] as u8;
                self.pixels[i + 1] = texel[1] as u8;
                self.pixels[i + 2] = texel[2] as u8;
                self.pixels[i + 3] = 255;
            }
        }
    }

    fn band(&mut self, center_y: f32, angle_deg: f32, thickness: f32, c: Rgba) {
        let angle = angle_deg.to_radians();
        let (sin, cos) = angle.sin_cos();
        let half = thickness / 2.0;
        let steps = thickness.ceil().max(1.0) as u32;
        for s in 0..steps {
            let offset = s as f32 - half;
            let normalized = offset / half;
            let falloff = (-normalized * normalized * 2.6).exp();
            if falloff <= 0.002 {
                continue;
            }
            let alpha = (c.0[3] as f32 * falloff) as u8;
            if alpha == 0 {
                continue;
            }
            let color = Rgba([c.0[0], c.0[1], c.0[2], alpha]);
            let cx = self.width as f32 / 2.0 + offset * -sin;
            let cy = center_y + offset * cos;
            let dx = cos * self.width as f32;
            let dy = sin * self.width as f32;
            add_pixel_line(self, cx - dx, cy - dy, cx + dx, cy + dy, color);
        }
    }

    fn grain(&mut self, density: u32, strength: f32, seed: u32) {
        let amount = ((strength.clamp(0.0, 1.0)) * 255.0) as u8;
        for i in 0..density {
            let h = hash(seed.wrapping_mul(2_654_435_761).wrapping_add(i));
            let x = (h & 0xffff) % self.width;
            let y = ((h >> 16) & 0xffff) % self.height;
            let bright = ((h >> 8) & 1) == 1;
            let value = if bright { amount } else { 0 };
            let c = Rgba([value, value, value, (amount as f32 * 0.5) as u8]);
            self.add_pixel(x, y, c);
        }
    }

    fn fill_polygon(&mut self, points: &[[f32; 2]], c: Rgba, additive: bool) {
        if points.len() < 3 || c.0[3] == 0 {
            return;
        }
        let min_y = points.iter().map(|p| p[1]).fold(f32::INFINITY, f32::min).max(0.0);
        let max_y = points.iter().map(|p| p[1]).fold(f32::NEG_INFINITY, f32::max).min(self.height as f32 - 1.0);
        if max_y <= min_y {
            return;
        }
        let samples = 4u32;
        let mut xs: Vec<f32> = Vec::with_capacity(16);
        // Coverage per pixel in 1/255 units is counted vertically by the
        // supersample passes and horizontally by the fractional span, so a
        // sloped polygon edge lands on a soft run of pixels instead of a stair.
        let mut coverage = vec![0u32; self.width as usize];
        for row in min_y.floor() as u32..=max_y.ceil() as u32 {
            if row >= self.height {
                break;
            }
            coverage.fill(0);
            for s in 0..samples {
                let y = row as f32 + (s as f32 + 0.5) / samples as f32;
                xs.clear();
                for i in 0..points.len() {
                    let a = points[i];
                    let b = points[(i + 1) % points.len()];
                    if (a[1] <= y) == (b[1] <= y) {
                        continue;
                    }
                    let t = (y - a[1]) / (b[1] - a[1]);
                    xs.push(a[0] + (b[0] - a[0]) * t);
                }
                if xs.is_empty() {
                    continue;
                }
                xs.sort_by(|p, q| p.partial_cmp(q).unwrap_or(std::cmp::Ordering::Equal));
                // Even-odd rule: fill between consecutive crossings.
                for pair in xs.as_chunks::<2>().0 {
                    let (x0, x1) = (pair[0], pair[1]);
                    if x1 <= x0 {
                        continue;
                    }
                    let start = x0.max(0.0).floor() as u32;
                    let end = x1.min(self.width as f32 - 1.0).floor() as u32;
                    for px in start..=end {
                        let left = (px as f32).max(x0);
                        let right = ((px + 1) as f32).min(x1);
                        if right > left {
                            let add = ((right - left) * 255.0).round() as u32;
                            coverage[px as usize] = coverage[px as usize].saturating_add(add);
                        }
                    }
                }
            }
            for px in 0..self.width {
                let covered = coverage[px as usize] as f32;
                if covered <= 0.0 {
                    continue;
                }
                let alpha = c.0[3] as f32 * covered / (samples as f32 * 255.0);
                if alpha < 1.0 {
                    continue;
                }
                let color = Rgba([c.0[0], c.0[1], c.0[2], alpha.min(255.0) as u8]);
                if additive {
                    self.add_pixel(px, row, color);
                }
                else {
                    self.blend(px, row, color);
                }
            }
        }
    }

    fn stroke_polyline(&mut self, points: &[[f32; 2]], width: f32, c: Rgba, progress: f32, additive: bool) {
        if points.len() < 2 || c.0[3] == 0 {
            return;
        }
        let progress = progress.clamp(0.0, 1.0);
        let mut total = 0.0f32;
        for i in 1..points.len() {
            total += distance(points[i - 1], points[i]);
        }
        if total <= 0.0 {
            return;
        }
        let limit = total * progress;
        if limit <= 0.0 {
            // At progress 0 the stroke does not exist yet: no caps, no dots.
            return;
        }
        let mut walked = 0.0f32;
        let half = (width / 2.0).max(0.5);
        // Round caps and joints come from soft discs, the body of every segment
        // from its distance field, so the outline stays smooth on any slope.
        stamp_disc(self, points[0][0], points[0][1], half, c, additive);
        let mut last = points[0];
        for i in 1..points.len() {
            let (a, b) = (points[i - 1], points[i]);
            let segment = distance(a, b);
            if segment <= 0.0001 {
                continue;
            }
            let usable = (limit - walked).min(segment);
            if usable <= 0.0 {
                break;
            }
            let t = usable / segment;
            let end = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
            stroke_segment(self, a, end, half, c, additive);
            last = end;
            if usable < segment {
                break;
            }
            if i + 1 < points.len() {
                stamp_disc(self, b[0], b[1], half, c, additive);
            }
            walked += usable;
            if walked >= limit {
                break;
            }
        }
        stamp_disc(self, last[0], last[1], half, c, additive);
    }

    fn circle(&mut self, cx: f32, cy: f32, radius: f32, c: Rgba, stroke: Option<f32>) {
        self.ellipse(cx, cy, radius, radius, c, stroke);
    }

    fn ellipse(&mut self, cx: f32, cy: f32, rx: f32, ry: f32, c: Rgba, stroke: Option<f32>) {
        let (rx, ry) = (rx.max(0.0), ry.max(0.0));
        if rx <= 0.0 || ry <= 0.0 || c.0[3] == 0 {
            return;
        }
        let pad = rx.max(ry) + 1.0;
        let x0 = (cx - pad).max(0.0) as u32;
        let y0 = (cy - pad).max(0.0) as u32;
        let x1 = (cx + pad).min(self.width as f32 - 1.0) as u32;
        let y1 = (cy + pad).min(self.height as f32 - 1.0) as u32;
        // A stroke wider than the narrow axis leaves no interior.
        let inner = stroke.map(|s| (rx - s, ry - s)).filter(|(ix, iy)| *ix > 0.0 && *iy > 0.0);
        for py in y0..=y1 {
            for px in x0..=x1 {
                let dx = px as f32 - cx;
                let dy = py as f32 - cy;
                let mut cover = ellipse_cover(dx, dy, rx, ry) + 0.5;
                if let Some((ix, iy)) = inner {
                    cover = cover.min(-ellipse_cover(dx, dy, ix, iy) + 0.5);
                }
                blend_cover(self, px, py, c, cover, false);
            }
        }
    }

    fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, thickness: f32, c: Rgba) {
        if c.0[3] == 0 {
            return;
        }
        let half = (thickness / 2.0).max(0.5);
        stroke_segment(self, [x0, y0], [x1, y1], half, c, false);
        stamp_disc(self, x0, y0, half, c, false);
        stamp_disc(self, x1, y1, half, c, false);
    }

    fn glow(&mut self, cx: f32, cy: f32, radius: f32, c: Rgba) {
        let x0 = (cx - radius).max(0.0) as u32;
        let y0 = (cy - radius).max(0.0) as u32;
        let x1 = (cx + radius).min(self.width as f32 - 1.0) as u32;
        let y1 = (cy + radius).min(self.height as f32 - 1.0) as u32;
        for py in y0..=y1 {
            for px in x0..=x1 {
                let dx = px as f32 - cx;
                let dy = py as f32 - cy;
                let d = (dx * dx + dy * dy).sqrt() / radius;
                if d >= 1.0 {
                    continue;
                }
                let falloff = (1.0 - d) * (1.0 - d);
                let a = c.0[3] as f32 * falloff / 255.0;
                add(self, px, py, Rgba([c.0[0], c.0[1], c.0[2], (a * 255.0) as u8]));
            }
        }
    }

    fn draw_bitmap(&mut self, bitmap: &Bitmap, x: f32, y: f32, w: f32, h: f32, fit: Fit, alpha: f32, radius: f32) {
        if w <= 0.0 || h <= 0.0 || bitmap.width == 0 || bitmap.height == 0 {
            return;
        }
        let (src_w, src_h) = (bitmap.width as f32, bitmap.height as f32);
        // Scale factors between the source and the destination box.
        let (sx, sy) = match fit {
            Fit::Stretch => (src_w / w, src_h / h),
            Fit::Contain | Fit::Cover => {
                let scale = if fit == Fit::Contain { (w / src_w).min(h / src_h) } else { (w / src_w).max(h / src_h) };
                let drawn_w = src_w * scale;
                let drawn_h = src_h * scale;
                // Undrawn area is centered; for cover the overflow is cropped.
                (src_w / drawn_w, src_h / drawn_h)
            }
        };
        let drawn_w = src_w / sx;
        let drawn_h = src_h / sy;
        let ox = x + (w - drawn_w) / 2.0;
        let oy = y + (h - drawn_h) / 2.0;

        let clip_x0 = x.max(0.0) as u32;
        let clip_y0 = y.max(0.0) as u32;
        let clip_x1 = (x + w).min(self.width as f32) as u32;
        let clip_y1 = (y + h).min(self.height as f32) as u32;
        let r = radius.min(w / 2.0).min(h / 2.0);
        for py in clip_y0..clip_y1 {
            for px in clip_x0..clip_x1 {
                if r > 0.01 {
                    let fx = px as f32 + 0.5;
                    let fy = py as f32 + 0.5;
                    let cx = fx.clamp(x + r, x + w - r);
                    let cy = fy.clamp(y + r, y + h - r);
                    let dx = fx - cx;
                    let dy = fy - cy;
                    if dx * dx + dy * dy > r * r {
                        continue;
                    }
                }
                let u = (px as f32 + 0.5 - ox) / drawn_w;
                let v = (py as f32 + 0.5 - oy) / drawn_h;
                if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
                    continue;
                }
                let texel = bitmap.sample(u * (src_w - 1.0), v * (src_h - 1.0));
                let a = (texel[3] / 255.0) * alpha;
                if a <= 0.001 {
                    continue;
                }
                self.blend(px, py, Rgba([texel[0] as u8, texel[1] as u8, texel[2] as u8, (a * 255.0) as u8]));
            }
        }
    }

    fn starfield(&mut self, count: u32, tint: Rgba, drift_px_per_sec: f32, twinkle: f32, seconds: f32) {
        for i in 0..count {
            let h = hash(i);
            let base_x = (h & 0xffff) as f32 / 65535.0;
            let base_y = ((h >> 16) & 0xffff) as f32 / 65535.0;
            let phase = ((h >> 3) & 255) as f32 / 255.0 * std::f32::consts::PI * 2.0;
            let speed = 0.4 + ((h >> 11) & 63) as f32 / 63.0 * 1.4;
            let size = 1.0 + ((h >> 20) & 3) as f32;
            let x = (base_x * self.width as f32 + drift_px_per_sec * seconds).rem_euclid(self.width as f32);
            let y = base_y * self.height as f32;
            let brightness = (1.0 - twinkle + twinkle * (0.5 + 0.5 * (seconds * speed + phase).sin())).clamp(0.0, 1.0);
            let c = Rgba([tint.0[0], tint.0[1], tint.0[2], (brightness * 230.0) as u8]);
            if size > 2.0 {
                self.glow(x, y, size * 3.0, c.with_alpha(brightness * 60.0));
            }
            self.rect(x, y, size, size, c);
        }
    }
}

/// Euclidean distance between two points.
#[must_use]
pub fn distance(a: [f32; 2], b: [f32; 2]) -> f32 {
    let dx = b[0] - a[0];
    let dy = b[1] - a[1];
    (dx * dx + dy * dy).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use media_image::Frame;

    fn red(frame: &Frame, x: u32, y: u32) -> u8 {
        frame.pixels[((y * frame.width + x) as usize) * 4]
    }

    fn partial_pixels(frame: &Frame) -> u32 {
        let mut partial = 0;
        for y in 0..frame.height {
            for x in 0..frame.width {
                let v = red(frame, x, y);
                if v > 8 && v < 247 {
                    partial += 1;
                }
            }
        }
        partial
    }

    /// A sloped stroke must feather its edge. Without coverage the border is
    /// either fully painted or not at all, which is exactly the staircase a
    /// viewer reads as "no anti-aliasing".
    #[test]
    fn polyline_edges_are_antialiased() {
        let mut frame = Frame::new(64, 64);
        frame.fill(Rgba([0, 0, 0, 255]));
        frame.stroke_polyline(&[[6.0, 6.0], [58.0, 58.0]], 3.0, Rgba([255, 255, 255, 255]), 1.0, false);
        assert!(partial_pixels(&frame) > 20, "sloped stroke must feather its edge");
        assert_eq!(red(&frame, 32, 32), 255, "the centre of the stroke stays solid");
    }

    /// A sloped polygon edge is feathered the same way by the horizontal
    /// coverage term, so filled shapes do not stair-step either.
    #[test]
    fn polygon_edges_are_antialiased() {
        let mut frame = Frame::new(64, 64);
        frame.fill(Rgba([0, 0, 0, 255]));
        frame.fill_polygon(&[[4.0, 4.0], [60.0, 22.0], [4.0, 60.0]], Rgba([255, 255, 255, 255]), false);
        assert!(partial_pixels(&frame) > 10, "sloped polygon edge must feather");
    }

    /// A ring keeps its stroke width and smooth outline; the interior stays
    /// untouched so the edge is the only soft part.
    #[test]
    fn ring_edges_are_antialiased() {
        let mut frame = Frame::new(64, 64);
        frame.fill(Rgba([0, 0, 0, 255]));
        frame.circle(32.0, 32.0, 24.0, Rgba([255, 255, 255, 255]), Some(4.0));
        assert!(partial_pixels(&frame) > 10, "ring edge must feather");
        assert_eq!(red(&frame, 32, 32), 0, "the ring leaves its interior empty");
    }

    /// Both radii of an ellipse reach the silhouette: the wide axis is painted
    /// out to `rx` and the flat axis stops at `ry`.
    #[test]
    fn ellipse_honours_both_radii() {
        let mut frame = Frame::new(64, 64);
        frame.fill(Rgba([0, 0, 0, 255]));
        frame.ellipse(32.0, 32.0, 28.0, 12.0, Rgba([255, 255, 255, 255]), None);
        assert!(red(&frame, 58, 32) > 200, "the wide axis reaches `rx`");
        assert_eq!(red(&frame, 32, 50), 0, "the flat axis stops at `ry`");
        assert!(partial_pixels(&frame) > 10, "the ellipse edge must feather");
    }

    /// A stroke follows the elliptical outline, so the ring leaves an elliptical
    /// hole instead of falling back to a circular one.
    #[test]
    fn elliptical_ring_keeps_its_outline() {
        let mut frame = Frame::new(64, 64);
        frame.fill(Rgba([0, 0, 0, 255]));
        frame.ellipse(32.0, 32.0, 28.0, 12.0, Rgba([255, 255, 255, 255]), Some(3.0));
        assert_eq!(red(&frame, 32, 32), 0, "the ring leaves its interior empty");
        assert!(red(&frame, 58, 32) > 200, "the wide edge is painted");
        assert_eq!(red(&frame, 32, 50), 0, "outside the flat edge stays empty");
    }
}

/// Resamples a polyline as a Catmull-Rom curve, so a handful of control points
/// becomes a drawn line. `segments` is the resolution between two points.
#[must_use]
pub fn smooth_polyline(points: &[[f32; 2]], closed: bool, segments: u32) -> Vec<[f32; 2]> {
    if points.len() < 2 {
        return points.to_vec();
    }
    let count = points.len();
    let at = |i: isize| -> [f32; 2] {
        let index = if closed { ((i % count as isize) + count as isize) % count as isize } else { i.clamp(0, count as isize - 1) };
        points[index as usize]
    };
    let mut out = Vec::with_capacity(count * segments as usize + 1);
    let last = if closed { count as isize } else { count as isize - 1 };
    for i in 0..last {
        let (p0, p1, p2, p3) = (at(i - 1), at(i), at(i + 1), at(i + 2));
        for s in 0..segments {
            let t = s as f32 / segments as f32;
            let t2 = t * t;
            let t3 = t2 * t;
            let mut point = [0.0f32; 2];
            for k in 0..2 {
                point[k] = 0.5
                    * ((2.0 * p1[k])
                        + (-p0[k] + p2[k]) * t
                        + (2.0 * p0[k] - 5.0 * p1[k] + 4.0 * p2[k] - p3[k]) * t2
                        + (-p0[k] + 3.0 * p1[k] - 3.0 * p2[k] + p3[k]) * t3);
            }
            out.push(point);
        }
    }
    if closed {
        out.push(out[0]);
    }
    else {
        out.push(points[count - 1]);
    }
    out
}
