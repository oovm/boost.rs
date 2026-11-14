use hardware_capability::{FeatureSet, WiredStrategies};
use hardware_ir::{Graph, ResourceId};
use hardware_scheduler::{Backend, Result};
use hardware_types::TaskNode;

/// The only failure a non-Windows DX12 handle can report.
fn unsupported() -> hardware_scheduler::Error {
    hardware_scheduler::Error::Backend("dx12".into(), "DirectX 12 backend requires Windows".into())
}

/// Non-Windows stub: DX12 is only available on Windows.
pub struct Dx12Backend {
    strategies: WiredStrategies,
}

impl Dx12Backend {
    /// Returns a non-Windows stub backend handle.
    pub fn new() -> Result<Self> {
        Ok(Self { strategies: WiredStrategies::from_features(&FeatureSet::new()) })
    }

    /// Degrade-chain strategies resolved when the backend opened.
    pub fn strategies(&self) -> WiredStrategies {
        self.strategies
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
}

impl Default for Dx12Backend {
    fn default() -> Self {
        Self::new().expect("dx12 stub")
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
