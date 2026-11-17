//! Unified Compute Fabric — public entry point.
//!
//! ```rust
//! use hardware::prelude::*;
//! ```
#![warn(missing_docs)]

pub use hardware_capability as capability;
pub use hardware_harness as harness;
pub use hardware_ir as ir;
pub use hardware_optimize as optimize;
pub use hardware_runtime as runtime;
pub use hardware_scheduler as scheduler;
pub use hardware_types as types;

// Shared value types
pub use hardware_types::{
    Access, DepKind, Dispatch, Domain, Objective, ParamValue, Priority, ResourceId, ResourceKind, ResourceNode, ShaderId, TaskId, TaskKind,
    TaskNode,
};

// Graph IR
pub use hardware_ir::{
    DepEdge, Error as IrError, Graph, GraphBuilder, MAGIC as UCF_MAGIC, ResourceGraph, Result as IrResult, TaskGraph,
    WIRE_MAJOR as UCF_WIRE_MAJOR, chain_edges, decode, encode,
};

// Capability / optimization
pub use hardware_capability::{
    BackendCapabilities, CapabilityReport, DescriptorStrategy, Feature, FeatureSet, PipelineStrategy, SyncStrategy, pick_descriptor_strategy,
    pick_pipeline_strategy, pick_sync_strategy,
};
pub use hardware_optimize::{
    AttentionMetrics, BackendPerfRow, MatmulMetrics, Optimization, PerfReport, TimedSample, apply, attention_flops, matmul_flops,
};

// Scheduler / runtime
pub use hardware_runtime::{CapacityPolicy, ExecutionDiagnostics, ExecutionEvent, ExecutionEventBuilder, Runtime, dry_run, event_kinds};
pub use hardware_scheduler::{
    Backend, Error as SchedulerError, ErrorCode as SchedulerErrorCode, ExecEvent, ExecStream, ExecutionBindings, ExternalBuffer,
    FRAME_BUDGET_120HZ_MICROS, ImmediateBridge, ImmediateEvent, ImmediateStream, NoopBackend, Result as SchedulerResult, Scheduler,
    StreamEventBridge, fits_120hz_frame,
};

#[cfg(feature = "cpu")]
mod session;

#[cfg(feature = "cpu")]
pub use session::{CpuSession, immediate_event, immediate_stream};

#[cfg(feature = "cpu")]
pub use hardware_backend_cpu as backend_cpu;

#[cfg(feature = "cuda")]
pub use hardware_backend_cuda as backend_cuda;

#[cfg(feature = "dx12")]
pub use hardware_backend_dx12 as backend_dx12;

#[cfg(feature = "vulkan")]
pub use hardware_backend_vulkan as backend_vulkan;

#[cfg(feature = "rocm")]
pub use hardware_backend_rocm as backend_rocm;

/// Common imports for application code.
pub mod prelude {
    pub use crate::{
        Access, AttentionMetrics, Backend, BackendCapabilities, BackendPerfRow, CapabilityReport, CapacityPolicy, DepEdge, DepKind,
        DescriptorStrategy, Dispatch, Domain, ExecEvent, ExecStream, ExecutionBindings, ExecutionDiagnostics, ExecutionEvent,
        ExecutionEventBuilder, ExternalBuffer, FRAME_BUDGET_120HZ_MICROS, Feature, FeatureSet, Graph, GraphBuilder, ImmediateBridge,
        ImmediateEvent, ImmediateStream, MatmulMetrics, NoopBackend, Objective, Optimization, ParamValue, PerfReport, PipelineStrategy,
        Priority, ResourceGraph, ResourceId, ResourceKind, ResourceNode, Runtime, Scheduler, SchedulerErrorCode, ShaderId, StreamEventBridge,
        SyncStrategy, TaskGraph, TaskId, TaskKind, TaskNode, TimedSample, UCF_MAGIC, UCF_WIRE_MAJOR, apply, attention_flops, chain_edges,
        decode, dry_run, encode, event_kinds, fits_120hz_frame, matmul_flops, pick_descriptor_strategy, pick_pipeline_strategy,
        pick_sync_strategy,
    };

    #[cfg(feature = "cpu")]
    pub use crate::backend_cpu::{CpuBackend, HostExternalBuffer, HostStore, shared_store};

    #[cfg(feature = "cpu")]
    pub use crate::{CpuSession, immediate_event, immediate_stream};

    #[cfg(feature = "cuda")]
    pub use crate::backend_cuda::CudaBackend;

    #[cfg(feature = "dx12")]
    pub use crate::backend_dx12::Dx12Backend;

    #[cfg(feature = "vulkan")]
    pub use crate::backend_vulkan::VulkanBackend;

    #[cfg(feature = "rocm")]
    pub use crate::backend_rocm::RocmBackend;
}
