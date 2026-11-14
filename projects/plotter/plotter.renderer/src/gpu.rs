//! Accelerated (GPU) rendering entry.
//!
//! Only the SVG backend and the CPU reference rasteriser are available. The
//! accelerated path has no runtime linked in this build, so requesting it fails
//! with [`GpuUnavailable`] instead of silently falling back, letting callers see
//! which backend actually served the frame.

use plotter_view::Scene;

use crate::RgbaImage;

/// The accelerated backend was requested but no runtime is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuUnavailable;

impl core::fmt::Display for GpuUnavailable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("accelerated rendering is unavailable, use the SVG or CPU backend")
    }
}

impl std::error::Error for GpuUnavailable {}

/// Whether an accelerated backend is linked into this build.
///
/// Always `false`: rendering is SVG / CPU only until an in-house accelerated
/// backend exists.
pub const fn is_available() -> bool {
    false
}

/// Render a scene through the accelerated backend.
///
/// Always returns [`GpuUnavailable`] in this build.
pub fn render(_scene: &Scene) -> Result<RgbaImage, GpuUnavailable> {
    Err(GpuUnavailable)
}
