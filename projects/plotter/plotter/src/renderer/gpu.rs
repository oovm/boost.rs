//! Accelerated (GPU) rendering entry.
//!
//! The default build links no accelerated runtime, so [`is_available`] is `false`
//! and [`render`] fails with [`GpuError::Unavailable`] instead of silently falling
//! back: callers always see which backend served the frame. Enabling the `dx12`
//! feature routes the scene through the in-house Direct3D 12 backend, which still
//! reports `Unavailable` when the host has no usable device.

use plotter_renderer::RgbaImage;
use plotter_view::Scene;

/// Accelerated rendering failure.
#[derive(Debug)]
pub enum GpuError {
    /// No accelerated backend is linked into this build, or the host has no usable device.
    Unavailable,
    /// The accelerated backend was available but could not render the scene.
    #[cfg(feature = "dx12")]
    Render(plotter_renderer_dx12::RenderError),
}

impl core::fmt::Display for GpuError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unavailable => f.write_str("accelerated rendering is unavailable, use the SVG or CPU backend"),
            #[cfg(feature = "dx12")]
            Self::Render(err) => write!(f, "accelerated rendering failed: {err}"),
        }
    }
}

impl std::error::Error for GpuError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unavailable => None,
            #[cfg(feature = "dx12")]
            Self::Render(err) => Some(err),
        }
    }
}

/// Whether an accelerated backend can serve this build on this host.
#[cfg(feature = "dx12")]
pub fn is_available() -> bool {
    plotter_renderer_dx12::is_available()
}

/// Whether an accelerated backend can serve this build on this host.
#[cfg(not(feature = "dx12"))]
pub fn is_available() -> bool {
    false
}

/// Render a scene at its viewport size through the accelerated backend.
///
/// Returns [`GpuError::Unavailable`] when no backend can serve the request, so a
/// GPU request never degrades into CPU output without the caller noticing.
pub fn render(scene: &Scene) -> Result<RgbaImage, GpuError> {
    #[cfg(feature = "dx12")]
    {
        if !plotter_renderer_dx12::is_available() {
            return Err(GpuError::Unavailable);
        }
        let width = scene.viewport.width.max(1.0).round() as u32;
        let height = scene.viewport.height.max(1.0).round() as u32;
        plotter_renderer_dx12::render_rgba8(scene, width, height).map_err(GpuError::Render)
    }
    #[cfg(not(feature = "dx12"))]
    {
        let _ = scene;
        Err(GpuError::Unavailable)
    }
}
