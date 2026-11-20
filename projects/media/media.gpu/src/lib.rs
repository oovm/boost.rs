//! GPU frame compositor (ADR 0005): wgpu + WGSL operators that reproduce the
//! CPU reference (`media.raster::Paint` / `media.post::Vignette`) operation by
//! operation on packed RGBA8 display bytes.
//!
//! The contract is display-space byte arithmetic: no sRGB conversion, no
//! float texture filtering. Pixels travel through a storage buffer of packed
//! `u32` (little-endian `r | g<<8 | b<<16 | a<<24`) so the shader sees exactly
//! the bytes the CPU reference sees.
//!
//! The `gpu` feature gates the backend itself: without it [`probe`] reports
//! [`GpuStatus::Disabled`] and [`GpuCompositor::new`] fails, leaving hosts to
//! the CPU path.

mod compositor;
mod probe;

pub use compositor::{GpuCompositor, GpuError};
pub use probe::{CompositorKind, GpuBackendInfo, GpuStatus, probe_gpu, probe_gpu_fallback};
