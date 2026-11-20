use hardware_capability::{FeatureSet, WiredStrategies};
use hardware_ir::{Graph, ResourceId};
use hardware_scheduler::{Backend, Result};
use hardware_types::TaskNode;

use crate::{AdapterInfo, ComputeAdapterInfo, ProgramCacheStats, SharedBufferId, Vertex};

/// The only failure a non-Windows DX12 handle can report.
fn unsupported() -> hardware_scheduler::Error {
    hardware_scheduler::Error::Backend("dx12".into(), "DirectX 12 backend requires Windows".into())
}

/// Non-Windows stub: DX12 is only available on Windows.
pub struct Dx12Backend {
    strategies: WiredStrategies,
}

impl Dx12Backend {
    /// DirectX 12 is unavailable off Windows.
    ///
    /// The stub used to hand out a handle whose every call failed; that made
    /// cross-platform callers treat the backend as usable and only discover
    /// otherwise mid-dispatch. Failing here lets them skip cleanly.
    pub fn new() -> Result<Self> {
        Err(unsupported())
    }

    /// Software-adapter fallback, equally unavailable off Windows.
    pub fn new_allowing_software() -> Result<Self> {
        Err(unsupported())
    }

    /// No DXGI adapter exists on a non-Windows host, which is an empty list
    /// rather than a failure: probing for adapters is a query.
    pub fn enumerate_adapters() -> Result<Vec<AdapterInfo>> {
        Ok(Vec::new())
    }

    /// No DXCore adapter exists on a non-Windows host.
    pub fn enumerate_compute_adapters() -> Result<Vec<ComputeAdapterInfo>> {
        Ok(Vec::new())
    }

    /// Degrade-chain strategies resolved when the backend opened.
    pub fn strategies(&self) -> WiredStrategies {
        self.strategies
    }

    /// No multisampled raster target is ever created off Windows.
    pub fn raster_samples(&self) -> u32 {
        1
    }

    /// Descriptor path label (always empty on the stub).
    pub fn last_descriptor_path(&self) -> &'static str {
        ""
    }

    /// Sync path label (always empty on the stub).
    pub fn last_sync_path(&self) -> &'static str {
        ""
    }

    /// Pipeline path label (always empty on the stub).
    pub fn last_pipeline_path(&self) -> &'static str {
        ""
    }

    /// No pipeline cache is ever populated off Windows.
    pub fn program_cache_stats(&self) -> ProgramCacheStats {
        ProgramCacheStats::default()
    }

    /// No cached pipeline to drop off Windows.
    pub fn clear_program_cache(&mut self) {}

    /// Shared-buffer allocation, rejected on non-Windows hosts.
    pub fn shared_alloc(&mut self, _bytes: usize) -> Result<(SharedBufferId, isize)> {
        Err(unsupported())
    }

    /// Shared-buffer upload, rejected on non-Windows hosts.
    pub fn shared_write_f32(&mut self, _id: SharedBufferId, _values: &[f32]) -> Result<()> {
        Err(unsupported())
    }

    /// Shared-buffer NT handle, rejected on non-Windows hosts.
    pub fn shared_nt_handle(&self, _id: SharedBufferId) -> Result<isize> {
        Err(unsupported())
    }

    /// Shared-buffer publication, rejected on non-Windows hosts.
    pub fn publish_shared(&mut self, _resource: ResourceId, _shared: SharedBufferId) -> Result<()> {
        Err(unsupported())
    }

    /// Buffer upload, rejected on non-Windows hosts.
    ///
    /// The methods mirror the Windows surface so cross-platform tests compile
    /// against one API; every call fails explicitly instead of pretending to
    /// run a device that does not exist.
    pub fn write_f32(&mut self, _id: ResourceId, _values: &[f32]) -> Result<()> {
        Err(unsupported())
    }

    /// Float readback, rejected on non-Windows hosts.
    pub fn read_f32(&mut self, _id: ResourceId) -> Result<Vec<f32>> {
        Err(unsupported())
    }

    /// Byte readback, rejected on non-Windows hosts.
    pub fn read_u8(&mut self, _id: ResourceId) -> Result<Vec<u8>> {
        Err(unsupported())
    }

    /// Prepared-graph execution, rejected on non-Windows hosts.
    pub fn run_prepared(&mut self, _graph: &Graph) -> Result<()> {
        Err(unsupported())
    }

    /// Vertex-buffer rasterization, rejected on non-Windows hosts.
    pub fn draw_vertex_buffer(&mut self, _vertices: &[Vertex], _width: u32, _height: u32) -> Result<Vec<u8>> {
        Err(unsupported())
    }
}

impl Default for Dx12Backend {
    fn default() -> Self {
        Self { strategies: WiredStrategies::from_features(&FeatureSet::new()) }
    }
}

impl Backend for Dx12Backend {
    fn name(&self) -> &str {
        "dx12"
    }

    fn features(&self) -> FeatureSet {
        FeatureSet::new()
    }

    fn submit_task(&mut self, _graph: &Graph, _task: &TaskNode) -> Result<()> {
        Err(hardware_scheduler::Error::Backend("dx12".into(), "DirectX 12 backend requires Windows".into()))
    }
}
