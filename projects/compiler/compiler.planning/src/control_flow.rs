//! Suspend artifacts carried from frontend to driver backends.
//!
//! Backends fall into two consumption models (owned by `compiler_target::backends`):
//! first-class continuations keep the language suspend plan intact, while the
//! state-machine model lowers to the explicit [`ControlFlowPayload`] below.

use compiler_types::{ItemId, QualifiedName};

/// First-class suspend payload: continuation plan without state-machine type emission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuspendRuntimePayload {
    /// Each suspend-capable function's runtime continuation artifact.
    pub functions: Vec<SuspendRuntimeFunctionArtifact>,
}

/// Serialized catch/resume continuation metadata for driver consumption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuspendContinuationArtifact {
    /// Stable continuation index inside the owning function.
    pub index: usize,
    /// Lane-neutral continuation carrier name.
    pub carrier: String,
    /// Handler dispatch block label in semantic MIR.
    pub dispatch_block_label: String,
    /// Resume block label after handler arm matching.
    pub resume_block_label: String,
    /// Handler exit merge block label.
    pub handler_exit_block_label: String,
    /// Number of resume parameters expected by the resume block.
    pub resume_parameter_count: usize,
}

/// Single suspend function artifact for first-class backends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuspendRuntimeFunctionArtifact {
    /// Stable symbol.
    pub symbol: QualifiedName,
    /// Entry block label in semantic MIR (pre-rewrite).
    pub entry_block_label: String,
    /// Frame slot field names shared across suspend states.
    pub frame_fields: Vec<String>,
    /// Per-suspend-point state metadata.
    pub states: Vec<SuspendStateArtifact>,
    /// Catch/resume continuation metadata.
    pub continuations: Vec<SuspendContinuationArtifact>,
}

/// 控制流载荷：suspend 分区提交给驱动层的结构化状态机描述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlFlowPayload {
    /// 每个可挂起函数的状态机 artifact。
    pub functions: Vec<SuspendFunctionArtifact>,
}

/// 单个 suspend 函数的状态机 artifact。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuspendFunctionArtifact {
    /// 稳定符号。
    pub symbol: QualifiedName,
    /// 嵌套 state-machine 类型名。
    pub state_machine_type: String,
    /// dispatch 读取的 state 字段名。
    pub state_field: String,
    /// frame 字段（含 spill slots，不含 state 字段）。
    pub frame_fields: Vec<String>,
    /// dispatch switch 映射。
    pub dispatch_cases: Vec<SuspendDispatchCase>,
    /// 各 suspend 状态描述。
    pub states: Vec<SuspendStateArtifact>,
    /// Catch/resume continuation metadata.
    pub continuations: Vec<SuspendContinuationArtifact>,
}

/// dispatch case：`case_key` → 目标 block 标签。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuspendDispatchCase {
    /// dispatch switch 的 case 键。
    pub case_key: u32,
    /// 跳转目标基本块标签。
    pub block_label: String,
}

/// Witness dispatch slot referenced by a suspend state (e.g. `Iterator::next`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuspendWitnessBinding {
    /// Trait protocol name (`Iterator`, `Future`, …).
    pub trait_name: String,
    /// Method name on the witness table.
    pub method_name: String,
    /// Frozen trait declaration identity when statically resolved.
    pub trait_declaration: Option<ItemId>,
    /// Frozen method declaration identity when statically resolved.
    pub method_declaration: Option<ItemId>,
    /// Stable slot index in the witness table.
    pub method_index: u32,
    /// Statically resolved implementing type when known.
    pub type_name: Option<String>,
    /// Diagnostic/export projection only; must not drive witness dispatch identity.
    pub impl_symbol: Option<String>,
}

/// 单个 suspend 状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuspendStateArtifact {
    /// 状态机内唯一状态标识。
    pub state_id: u32,
    /// 挂起时触发的效应名称。
    pub effect: String,
    /// 恢复后进入的 dispatch case 键。
    pub resume_case_key: u32,
    /// 承载帧数据的载体字段名。
    pub frame_carrier: String,
    /// 跨挂起需保留的 spill 槽字段名。
    pub spill_fields: Vec<String>,
    /// 挂起点基本块标签。
    pub suspend_block_label: String,
    /// 恢复后继续执行的基本块标签。
    pub resume_block_label: String,
    /// 恢复基本块期望的参数个数。
    pub resume_parameter_count: usize,
    /// Statically resolved witness calls consumed by this suspend state.
    pub witness_bindings: Vec<SuspendWitnessBinding>,
    /// Linked continuation index when suspend is an uncaught `Raise`.
    pub continuation_index: Option<usize>,
}
