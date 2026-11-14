//! In-house Direct3D 12 backend for `plotter-view` scenes.
//!
//! Scenes are converted into a pixel-space triangle list and rasterised by
//! [`hardware_backend_dx12`], whose DXBC is emitted inside this repository. Text
//! runs are rasterised on the CPU from system font files and composited over
//! the device frame. No external GPU runtime and no shader text are involved,
//! and nothing here is a silent fallback: an unavailable device, a missing font
//! or an unconvertible node is reported.

#![deny(missing_docs)]

mod error;
mod geometry;
mod text;

pub use error::RenderError;
use plotter_renderer::RgbaImage;
use plotter_view::Scene;

use std::sync::{Mutex, OnceLock};

/// Process-wide device so repeated renders reuse one `Direct3D 12` adapter.
///
/// The slot is cleared when a draw fails, which lets the next render rebuild the
/// device instead of returning the same stale failure forever.
fn device_slot() -> &'static Mutex<Option<hardware_backend_dx12::Dx12Backend>> {
    static DEVICE: OnceLock<Mutex<Option<hardware_backend_dx12::Dx12Backend>>> = OnceLock::new();
    DEVICE.get_or_init(|| Mutex::new(None))
}

/// Runs `draw` on the shared device, rebuilding it after a failure.
fn with_device<T>(draw: impl FnOnce(&mut hardware_backend_dx12::Dx12Backend) -> Result<T, RenderError>) -> Result<T, RenderError> {
    let mut slot = device_slot().lock().map_err(|_| RenderError::Backend("shared device lock is poisoned".into()))?;
    if slot.is_none() {
        *slot = Some(hardware_backend_dx12::Dx12Backend::new().map_err(RenderError::backend)?);
    }
    let backend = slot.as_mut().expect("the device slot was just filled");
    match draw(backend) {
        Ok(value) => Ok(value),
        Err(error) => {
            *slot = None;
            Err(error)
        }
    }
}

/// Whether this host exposes a usable Direct3D 12 device.
///
/// Always `false` outside Windows. The probe reuses the shared device.
pub fn is_available() -> bool {
    cfg!(windows) && device_slot().lock().map(|slot| slot.is_some() || hardware_backend_dx12::Dx12Backend::new().is_ok()).unwrap_or(false)
}

/// Rasterise `scene` into `width`x`height` RGBA8 pixels.
///
/// Triangles are drawn opaque over a transparent black background, then text
/// runs are composited over them, so pixels no node touches stay
/// `[0, 0, 0, 0]`. An empty scene returns an all-transparent image instead of an
/// error, and a text-only scene needs no device. Consecutive calls reuse one
/// device and one set of font faces.
pub fn render_rgba8(scene: &Scene, width: u32, height: u32) -> Result<RgbaImage, RenderError> {
    if width == 0 || height == 0 {
        return Err(RenderError::EmptyTarget { width, height });
    }
    let geometry = geometry::collect(scene, height)?;
    let mut pixels = if geometry.vertices.is_empty() {
        vec![0; (width as usize) * (height as usize) * 4]
    }
    else {
        with_device(|backend| backend.draw_vertex_buffer(&geometry.vertices, width, height).map_err(RenderError::backend))?
    };
    text::draw(&mut pixels, width, height, &geometry.texts)?;
    Ok(RgbaImage { width, height, pixels })
}
