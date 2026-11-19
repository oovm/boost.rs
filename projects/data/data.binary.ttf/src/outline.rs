//! Outline builder trait and bbox types — consumed by font-face convenience
//! methods to emit glyph outlines in a backend-agnostic way.
//!
//! The [`OutlineBuilder`] trait is deliberately small (move, line, quadratic,
//! cubic, close) so adapters such as SVG path serialization or rasterization
//! can share one shape across the `glyf` and CFF backends.

/// Minimal bounding box in font units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BBox {
    pub x_min: i16,
    pub y_min: i16,
    pub x_max: i16,
    pub y_max: i16,
}

/// Backend-agnostic glyph-outline consumer.
///
/// Implementations receive normalized outline segments one at a time and are
/// responsible for accumulating whatever representation they need (SVG path,
/// PDF operators, rasterization coverage...)
pub trait OutlineBuilder {
    fn move_to(&mut self, x: f32, y: f32);
    fn line_to(&mut self, x: f32, y: f32);
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32);
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32);
    fn close(&mut self);
}
