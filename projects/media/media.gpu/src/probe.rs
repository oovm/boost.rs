//! Adapter probe and the `MEDIA_COMPOSITOR=auto|gpu|cpu` preference contract.
//!
//! `auto` prefers GPU and falls back to CPU when no adapter exists; `gpu`
//! makes a missing adapter a hard error in the caller; `cpu` never touches
//! the backend. A silent CPU fallback is never acceptable — the caller must
//! report the degradation (ADR 0005).

use serde::Serialize;
use std::env;

/// Which compositor the host asked for, before probing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositorPreference {
    /// GPU when an adapter exists, CPU otherwise (must be reported).
    Auto,
    /// GPU only; a missing adapter is an error.
    Gpu,
    /// CPU only; the backend is never touched.
    Cpu,
}

/// The effective compositor kind after preference and probe are resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompositorKind {
    /// The wgpu backend.
    Gpu,
    /// The CPU reference path.
    Cpu,
}

/// Whether a usable adapter was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuStatus {
    /// An adapter can be acquired.
    Available,
    /// No adapter exists (or acquisition failed).
    Unavailable,
    /// The backend is off: `gpu` feature disabled or `MEDIA_COMPOSITOR=cpu`.
    Disabled,
}

/// Facts about the GPU backend on this host.
#[derive(Debug, Clone, Serialize)]
pub struct GpuBackendInfo {
    /// Probe outcome.
    pub status: GpuStatus,
    /// The compositor the preference resolves to.
    pub preferred: CompositorKind,
    /// Adapter name, when available.
    pub adapter_name: Option<String>,
    /// wgpu backend (Vulkan/Dx12/...), when available.
    pub backend: Option<String>,
    /// Device type (DiscreteGpu/...), when available.
    pub device_type: Option<String>,
    /// Human-readable probe result.
    pub message: String,
}

/// `MEDIA_COMPOSITOR=auto|gpu|cpu` (default `auto`).
#[must_use]
pub fn compositor_preference() -> CompositorPreference {
    match env::var("MEDIA_COMPOSITOR").unwrap_or_else(|_| "auto".into()).to_ascii_lowercase().as_str() {
        "cpu" => CompositorPreference::Cpu,
        "gpu" => CompositorPreference::Gpu,
        _ => CompositorPreference::Auto,
    }
}

/// Resolve the effective compositor after preference + probe.
#[must_use]
pub fn preferred_compositor() -> CompositorKind {
    match compositor_preference() {
        CompositorPreference::Cpu => CompositorKind::Cpu,
        CompositorPreference::Gpu => CompositorKind::Gpu,
        CompositorPreference::Auto => match probe_gpu().status {
            GpuStatus::Available => CompositorKind::Gpu,
            _ => CompositorKind::Cpu,
        },
    }
}

/// Probe for a high-performance adapter.
#[must_use]
#[cfg(feature = "gpu")]
pub fn probe_gpu() -> GpuBackendInfo {
    match compositor_preference() {
        CompositorPreference::Cpu => return GpuBackendInfo {
            status: GpuStatus::Disabled,
            preferred: CompositorKind::Cpu,
            adapter_name: None,
            backend: None,
            device_type: None,
            message: "GPU compositor disabled via MEDIA_COMPOSITOR=cpu".into(),
        },
        CompositorPreference::Gpu | CompositorPreference::Auto => {}
    }
    request_adapter_info(false)
}

/// Probe the fallback adapter (software rasterizers such as WARP). This is
/// what parity tests use on machines without a real GPU.
#[must_use]
#[cfg(feature = "gpu")]
pub fn probe_gpu_fallback() -> GpuBackendInfo {
    request_adapter_info(true)
}

#[cfg(feature = "gpu")]
fn request_adapter_info(force_fallback: bool) -> GpuBackendInfo {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        ..Default::default()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: force_fallback,
    }));
    match adapter {
        Ok(adapter) => {
            let info = adapter.get_info();
            GpuBackendInfo {
                status: GpuStatus::Available,
                preferred: CompositorKind::Gpu,
                adapter_name: Some(info.name),
                backend: Some(format!("{:?}", info.backend)),
                device_type: Some(format!("{:?}", info.device_type)),
                message: "wgpu adapter available".into(),
            }
        }
        Err(error) => GpuBackendInfo {
            status: GpuStatus::Unavailable,
            preferred: CompositorKind::Cpu,
            adapter_name: None,
            backend: None,
            device_type: None,
            message: format!("no wgpu adapter: {error}"),
        },
    }
}

/// Probe without the backend compiled in: always [`GpuStatus::Disabled`].
#[must_use]
#[cfg(not(feature = "gpu"))]
pub fn probe_gpu() -> GpuBackendInfo {
    GpuBackendInfo {
        status: GpuStatus::Disabled,
        preferred: CompositorKind::Cpu,
        adapter_name: None,
        backend: None,
        device_type: None,
        message: "media-gpu built without `gpu` feature".into(),
    }
}

/// The fallback probe without the backend compiled in.
#[must_use]
#[cfg(not(feature = "gpu"))]
pub fn probe_gpu_fallback() -> GpuBackendInfo {
    probe_gpu()
}
