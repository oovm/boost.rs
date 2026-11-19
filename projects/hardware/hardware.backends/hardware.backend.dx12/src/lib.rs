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

#[cfg(windows)]
pub use win::{AdapterInfo, Dx12Backend, ProgramCacheStats, SharedBufferId, Vertex};

#[cfg(not(windows))]
pub use stub::{Dx12Backend, Vertex};
