//! GPU frame compositor (ADR 0005): D3D12 compute operators that reproduce the
//! CPU reference (`media.raster::Paint` / `media.post::Vignette`) operation by
//! operation on packed RGBA8 display bytes.
//!
//! The contract is display-space byte arithmetic: no sRGB conversion, no
//! hardware texture filtering. Pixels travel through a storage buffer of
//! packed `u32` (little-endian `r | g<<8 | b<<16 | a<<24`) so the shader sees
//! exactly the bytes the CPU reference sees. Each operator is a UCF program
//! emitted to DXBC by `hardware.emitter` and dispatched through the in-tree
//! `hardware.backend.dx12`; no third-party GPU runtime is on the production
//! path (合同 14 §1.2).
//!
//! The `dx12` feature gates the backend itself: without it [`probe`] reports
//! [`GpuStatus::Disabled`] and [`GpuCompositor::new`] fails, leaving hosts to
//! the CPU path. The `wgsl` feature instead emits every kernel as WGSL with a
//! binding manifest ([`shader_bundles`]), for Web/WASM hosts that cannot use
//! D3D12.

#[cfg(feature = "wgsl")]
mod bundle;
#[cfg(feature = "dx12")]
mod compositor;
mod kernels;
mod probe;

#[cfg(feature = "wgsl")]
pub use bundle::{BundleError, FRAME_ELEMENTS, STOP_ELEMENTS, ShaderBundle, ShaderManifest, shader_bundles};
#[cfg(feature = "dx12")]
pub use compositor::{GpuCompositor, GpuError};
pub use probe::{CompositorKind, GpuBackendInfo, GpuStatus, probe_gpu, probe_gpu_fallback};
