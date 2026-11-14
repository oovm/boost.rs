//! Optimization hint descriptors (auto-tune skeleton).
#![warn(missing_docs)]

mod metrics;
mod optimization;
mod perf;
mod program;

pub use metrics::{AttentionMetrics, MatmulMetrics, attention_flops, matmul_flops};
pub use optimization::Optimization;
pub use perf::{BackendPerfRow, PerfReport, TimedSample};
pub use program::{OptimizeOptions, is_foldable_scalar, optimize_program};

use hardware_ir::{Graph, Result};

/// Applies optimization hints to a graph. Skeleton: no-op until auto-tune lands.
pub fn apply(graph: &mut Graph, _opts: &[Optimization]) -> Result<()> {
    graph.validate()?;
    Ok(())
}
