//! Adapter probes independent of the host's compositor policy.

use serde::Serialize;
/// The compositor kind suggested by the probe.
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
    /// The backend was built without the `gpu` feature.
    Disabled,
}

/// Facts about the GPU backend on this host.
#[derive(Debug, Clone, Serialize)]
pub struct GpuBackendInfo {
    /// Probe outcome.
    pub status: GpuStatus,
    /// The compositor suggested by adapter availability.
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

/// Probe for a high-performance adapter.
#[must_use]
#[cfg(feature = "gpu")]
pub fn probe_gpu() -> GpuBackendInfo {
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
