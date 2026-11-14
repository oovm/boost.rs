//! In-house Direct3D 12 backend for `plotter-view` scenes.
//!
//! Scenes are converted into a pixel-space triangle list and rasterised by
//! [`hardware_backend_dx12`], whose DXBC is emitted inside this repository. No
//! external GPU runtime and no shader text are involved, and nothing here is a
//! silent fallback: an unavailable device or an unconvertible node is reported.

#![deny(missing_docs)]

mod error;
mod geometry;

pub use error::RenderError;
use plotter_renderer::RgbaImage;
use plotter_view::Scene;

/// Whether this host exposes a usable Direct3D 12 device.
///
/// Always `false` outside Windows.
pub fn is_available() -> bool {
    cfg!(windows) && hardware_backend_dx12::Dx12Backend::new().is_ok()
}

/// Rasterise `scene` into `width`x`height` RGBA8 pixels.
///
/// Triangles are drawn opaque over a transparent black background, so pixels no
/// node touches stay `[0, 0, 0, 0]`. An empty scene returns an all-transparent
/// image instead of an error.
pub fn render_rgba8(scene: &Scene, width: u32, height: u32) -> Result<RgbaImage, RenderError> {
    if width == 0 || height == 0 {
        return Err(RenderError::EmptyTarget { width, height });
    }
    let vertices = geometry::collect_triangles(scene, height)?;
    if vertices.is_empty() {
        return Ok(RgbaImage { width, height, pixels: vec![0; (width as usize) * (height as usize) * 4] });
    }

    let mut backend = hardware_backend_dx12::Dx12Backend::new().map_err(RenderError::backend)?;
    let pixels = backend.draw_vertex_buffer(&vertices, width, height).map_err(RenderError::backend)?;
    Ok(RgbaImage { width, height, pixels })
}
