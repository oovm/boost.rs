use serde::{Deserialize, Serialize};

/// Hardware or API capability exposed by a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Feature {
    /// 通用程序能够实际执行 F32 标量算术，不代表任意程序或资源布局均受支持。
    ProgramFloat32,
    /// 通用程序能够实际执行 F64 标量算术，不能仅凭硬件支持就声明此能力。
    /// 资源布局、数学原语与控制流仍须分别通过后端验证。
    ProgramFloat64,
    // Compute
    CudaGraph,
    /// HIP graph capture / replay (AMD ROCm path).
    HipGraph,
    UnifiedMemory,
    TensorCore,
    /// AMD matrix cores (MFMA); distinct from NVIDIA `TensorCore`.
    MatrixCore,
    DynamicParallelism,
    CooperativeGroups,
    ClusterLaunch,
    /// NPU 能整体编译并执行固定形状的量化子图，而不是逐算子解释。
    NpuCompiledSubgraph,
    /// 后端支持分段装载，可把大于实时预算的模型按段换入换出。
    SegmentedLoading,
    // Graphics
    Bindless,
    DescriptorHeap,
    DescriptorBuffer,
    ShaderObject,
    PipelineLibrary,
    DynamicRendering,
    RayTracing,
    WorkGraphs,
    WorkLists,
    // Sync
    EnhancedBarriers,
    Synchronization2,
}
