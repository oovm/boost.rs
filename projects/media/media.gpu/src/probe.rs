//! Adapter probes independent of the host's compositor policy.

use serde::Serialize;
/// The compositor kind suggested by the probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompositorKind {
    /// The D3D12 backend.
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
    /// The backend was built without the `dx12` feature.
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
    /// Graphics backend (`D3D12`), when available.
    pub backend: Option<String>,
    /// Device type (discrete/integrated/software), when available.
    pub device_type: Option<String>,
    /// Human-readable probe result.
    pub message: String,
}

/// Probe for a hardware D3D12 adapter.
#[must_use]
#[cfg(all(feature = "dx12", windows))]
pub fn probe_gpu() -> GpuBackendInfo {
    adapter_info()
}

/// D3D12 has one device path; WARP is chosen by the OS when no hardware
/// adapter is present, so the fallback probe is the same query.
#[must_use]
#[cfg(all(feature = "dx12", windows))]
pub fn probe_gpu_fallback() -> GpuBackendInfo {
    adapter_info()
}

#[cfg(all(feature = "dx12", windows))]
fn adapter_info() -> GpuBackendInfo {
    match hardware_backend_dx12::Dx12Backend::enumerate_adapters() {
        Ok(adapters) if !adapters.is_empty() => {
            let adapter = adapters.iter().find(|adapter| !adapter.software).unwrap_or(&adapters[0]);
            let device_type = if adapter.software {
                "software"
            }
            else if adapter.dedicated_memory >= 1 << 30 {
                "discrete"
            }
            else {
                "integrated"
            };
            GpuBackendInfo {
                status: GpuStatus::Available,
                preferred: CompositorKind::Gpu,
                adapter_name: Some(adapter.name.clone()),
                backend: Some("D3D12".into()),
                device_type: Some(device_type.into()),
                message: "D3D12 adapter available".into(),
            }
        }
        Ok(_) => GpuBackendInfo {
            status: GpuStatus::Unavailable,
            preferred: CompositorKind::Cpu,
            adapter_name: None,
            backend: None,
            device_type: None,
            message: "no D3D12 adapter".into(),
        },
        Err(error) => GpuBackendInfo {
            status: GpuStatus::Unavailable,
            preferred: CompositorKind::Cpu,
            adapter_name: None,
            backend: None,
            device_type: None,
            message: format!("enumerate D3D12 adapters: {error}"),
        },
    }
}

/// The largest hardware adapter name, used for provenance labels.
#[cfg(all(feature = "dx12", windows))]
pub(crate) fn default_adapter_name() -> Option<String> {
    let adapters = hardware_backend_dx12::Dx12Backend::enumerate_adapters().ok()?;
    adapters
        .iter()
        .find(|adapter| !adapter.software)
        .or_else(|| adapters.first())
        .map(|adapter| adapter.name.clone())
}

#[cfg(all(feature = "dx12", not(windows)))]
pub(crate) fn default_adapter_name() -> Option<String> {
    None
}

/// Probe without the backend compiled in (or off Windows): not available.
#[must_use]
#[cfg(not(all(feature = "dx12", windows)))]
pub fn probe_gpu() -> GpuBackendInfo {
    GpuBackendInfo {
        status: if cfg!(feature = "dx12") { GpuStatus::Unavailable } else { GpuStatus::Disabled },
        preferred: CompositorKind::Cpu,
        adapter_name: None,
        backend: None,
        device_type: None,
        message: if cfg!(feature = "dx12") {
            "D3D12 backend requires Windows".into()
        }
        else {
            "media-gpu built without `dx12` feature".into()
        },
    }
}

/// The fallback probe without the backend compiled in.
#[must_use]
#[cfg(not(all(feature = "dx12", windows)))]
pub fn probe_gpu_fallback() -> GpuBackendInfo {
    probe_gpu()
}
