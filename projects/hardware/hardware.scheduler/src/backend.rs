use hardware_capability::{BackendId, DeviceId, DeviceState, ExecDecision, ExecPlan, ExecSignature, FeatureSet, Health, MemoryBudget};
use hardware_ir::{Graph, TaskNode};

use crate::{
    error::Result,
    exec::{ExecStream, ExecutionBindings, StreamEventBridge},
    probe::BackendProbeEvent,
};

/// Maps UCF IR onto a concrete API (CUDA, DX12, Vulkan, CPU, …).
pub trait Backend: Send + Sync {
    /// Stable backend id used by the scheduler for placement.
    fn name(&self) -> &str;

    /// Declared hardware / API capabilities.
    fn features(&self) -> FeatureSet;

    /// Stable id for this API path; defaults to [`Self::name`].
    fn id(&self) -> BackendId {
        BackendId::new(self.name())
    }

    /// Physical device this backend drives.
    ///
    /// Several backends may share one device (e.g. DX12 and Vulkan on the same GPU);
    /// they must return the same [`DeviceId`] so budgets are accounted once.
    fn device(&self) -> DeviceId {
        DeviceId::new(self.name())
    }

    /// Lifecycle state reported to placement; only [`DeviceState::Submittable`] is selectable.
    fn state(&self) -> DeviceState {
        DeviceState::Submittable
    }

    /// Health reported to placement; unhealthy devices are filtered out.
    fn health(&self) -> Health {
        Health::Healthy
    }

    /// Live memory budget for this device path.
    ///
    /// When several backends share one physical [`DeviceId`] (e.g. DX12 and Vulkan on the
    /// same GPU), each must report device-wide usage so placement does not double-book it.
    fn budget(&self) -> MemoryBudget {
        MemoryBudget::unlimited()
    }

    /// Whether this backend can execute `signature`, and at what cost.
    ///
    /// Default accepts every signature at zero cost, which keeps simple backends and
    /// existing tests working; real backends override this with their operator/shape limits.
    fn supports(&self, _signature: &ExecSignature) -> ExecDecision {
        ExecDecision::Accept(ExecPlan::new(self.id(), self.device()))
    }

    /// Adopt external buffers / preferred stream from a downstream adapter.
    ///
    /// Default: ignore bindings (backend allocates privately). CPU / CUDA
    /// backends override to honor DXO / Spark device-resident objects.
    fn bind_externals(&mut self, _bindings: &ExecutionBindings) -> Result<()> {
        Ok(())
    }

    /// Allocate or bind graph resources before tasks run.
    fn prepare(&mut self, _graph: &Graph) -> Result<()> {
        Ok(())
    }

    /// Execute one task after dependencies have completed.
    fn submit_task(&mut self, graph: &Graph, task: &TaskNode) -> Result<()>;

    /// Block until prior submits on this backend are visible to other backends.
    ///
    /// Called by the scheduler when the next task is placed on a different backend.
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    /// Optional stream/event bridge for cross-stream dependencies.
    fn stream_bridge(&self) -> Option<&dyn StreamEventBridge> {
        None
    }

    /// Stream currently preferred for submits (if any).
    fn active_stream(&self) -> Option<&dyn ExecStream> {
        None
    }

    /// Drain probe events accumulated since the last take (module cache, compile, …).
    fn take_probe_events(&mut self) -> Vec<BackendProbeEvent> {
        Vec::new()
    }
}
