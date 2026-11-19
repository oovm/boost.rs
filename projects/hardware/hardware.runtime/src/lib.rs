//! UCF runtime: backend registration and graph execution.
#![warn(missing_docs)]

mod capacity;
mod diagnostics;
mod runtime;
mod stream;

pub use capacity::{
    CapacityPolicy, ResourceMigration, apply_capacity, check_soft_limit, domain_bytes, migrate_overflow_to_host,
    migrate_overflow_to_host_logged,
};
pub use diagnostics::{ExecutionDiagnostics, ExecutionEvent, ExecutionEventBuilder, kinds as event_kinds};
pub use runtime::Runtime;
pub use stream::{LoadPlan, SegmentId, SegmentStreamer, StreamError, StreamStats, Tier, Transfer};

use hardware_ir::Graph;
use hardware_scheduler::{NoopBackend, Result};

/// Validate a graph and schedule it on the noop backend (no hardware).
pub fn dry_run(graph: &Graph) -> Result<()> {
    graph.validate().map_err(hardware_scheduler::Error::from)?;
    let mut runtime = Runtime::new();
    runtime.register_backend(Box::new(NoopBackend));
    runtime.run(graph)
}
