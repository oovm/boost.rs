//! Direct3D 12 compute backend.
//!
//! 运行时只依赖 Windows 自带的 `d3d12.dll` 与 DXGI，终端用户不需要安装
//! “Graphics Tools / DirectX 调试层” 可选功能。设置 `BOOST_DX12_DEBUG=1` 才会尝试启用
//! 调试层与 GPU 校验；该功能缺失时（没有 `d3d12sdklayers.dll`）自动退化为空操作，
//! 不影响正常提交与读回。
#![warn(missing_docs)]

#[cfg(windows)]
mod params;

#[cfg(windows)]
mod win;

#[cfg(not(windows))]
mod stub;

/// Opaque id for a DX12 buffer exported via NT shared handle (CUDA import).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SharedBufferId(pub u64);

/// One vertex of the vertex-buffer raster path.
///
/// `position` is in pixels of the render target with the origin at its top-left
/// corner, and `color` is straight (non-premultiplied) RGBA in `0..=1`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vertex {
    /// Pixel-space position.
    pub position: [f32; 2],
    /// RGBA colour.
    pub color: [f32; 4],
}

/// Lookup counters of the generic-program pipeline cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProgramCacheStats {
    /// Dispatches served from the cache.
    pub hits: u64,
    /// Dispatches that had to encode `DXBC` and build a pipeline state.
    pub misses: u64,
    /// Pipelines currently cached.
    pub entries: usize,
}

/// 一块对 DXGI 可见的适配器。
///
/// NPU 通过 MCDM 暴露时也会出现在这里（计算专用适配器），可用来确认设备是否
/// 真正接到了 D3D12，而不是只有一条 PCI 记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterInfo {
    /// 适配器描述。
    pub name: String,
    /// PCI 厂商 ID。
    pub vendor_id: u32,
    /// PCI 设备 ID。
    pub device_id: u32,
    /// 专用显存字节数。
    pub dedicated_memory: u64,
    /// 是否为软件（WARP）适配器。
    pub software: bool,
}

impl AdapterInfo {
    /// 是否为软件光栅器（WARP / Microsoft Basic Render Driver）。
    ///
    /// 判定同时看 `DXGI_ADAPTER_DESC1.Flags` 的 `SOFTWARE` 位、微软的厂商 ID
    /// `0x1414`，以及显示名里的 `Basic Render`/`WARP`：CI 上的
    /// `windows-latest` 只暴露 Microsoft Basic Render Driver，在它上面创建设备
    /// 或提交计算会直接把进程打成 `STATUS_ACCESS_VIOLATION`。
    #[must_use]
    pub fn is_software(&self) -> bool {
        self.software || classify_software(self.vendor_id, 0, &self.name)
    }
}

/// 微软把 WARP 与 Microsoft Basic Render Driver 都挂在厂商 ID `0x1414` 下。
const MICROSOFT_VENDOR_ID: u32 = 0x1414;

/// `DXGI_ADAPTER_FLAG_SOFTWARE` 的位值，`windows` crate 只在 Windows 目标可用，
/// 因此这里直接取规范里的固定值，保持与非 Windows 目标共用同一份判定。
const DXGI_ADAPTER_FLAG_SOFTWARE_BIT: u32 = 0x2;

/// 按 `DXGI_ADAPTER_DESC1` 的厂商 ID、`Flags` 与显示名判定软件适配器。
fn classify_software(vendor_id: u32, flags: u32, name: &str) -> bool {
    vendor_id == MICROSOFT_VENDOR_ID || flags & DXGI_ADAPTER_FLAG_SOFTWARE_BIT != 0 || name.contains("Basic Render") || name.contains("WARP")
}

/// 一块对 DXCore 可见的计算适配器；MCDM 设备（如 NPU）会出现在这里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputeAdapterInfo {
    /// 驱动描述。
    pub name: String,
    /// 硬件 ID。
    pub hardware_id: String,
    /// 专用适配器内存字节数。
    pub dedicated_memory: u64,
    /// 是否支持 D3D12 core compute。
    pub d3d12_core_compute: bool,
    /// 是否支持 D3D12 图形。
    pub d3d12_graphics: bool,
}

#[cfg(windows)]
pub use win::Dx12Backend;

#[cfg(not(windows))]
pub use stub::Dx12Backend;
