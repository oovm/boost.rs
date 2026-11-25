#![warn(missing_docs)]

//! 语言无关的编译期合同类型：语义身份、链接后程序事实、布局计划、可审计中立合同与诊断线格式。
//!
//! # 分层
//!
//! - 本 crate 只承载各编译阶段之间交换的**中立数据形状**，不含任何语言 AST/HIR/MIR 定义，
//!   也不含目标私有表示。
//! - 身份（[`SemanticId`] 家族）与源码区间协议（[`SourceSpan`] / [`SourceID`]）在此定义，
//!   供语言前端、分析与后端共同消费；快照、字节访问与文本索引仍由 `boost-source` 拥有。
//! - 诊断依赖渲染、收集与终端呈现分别由 `boost-harness` 的 `diagnostic` 模块持有；
//!   本模块的 [`DiagnosticRow`] 只定义跨语言线格式（feature `serde` 下可序列化），
//!   不实现收集器或渲染器。

pub use self::{
    compiler_input::{DeclarationIndexEntry, DeclarationKey, DeclarationKind, DeclarationLink, EntryPointKind, ReferencePath},
    contract_versions::{IDENTITY_SCHEMA_VERSION, LAYOUT_PLAN_VERSION, MIR_CONTRACT_VERSION, contract_version_fingerprint},
    diagnostics::{
        DiagnosticArg, DiagnosticOwner, DiagnosticOwnerClass, DiagnosticPhase, DiagnosticRow, DiagnosticSeverity, DiagnosticSource,
        DiagnosticSpan,
    },
    digest::combined_content_hash,
    errors::{TypesError, TypesErrorKind},
    external_import::{ExternalCallArgument, ExternalCallEdge, ExternalImportLink, InternalCallEdge},
    layout::{
        AggregateLayout, AggregateLayoutPlan, FieldLayout, FlagsLayout, LayoutId, LayoutKey, SINGLETON_CONSTRUCTOR_NAME,
        SINGLETON_EAGER_ACCESSOR, SINGLETON_FINALIZER_NAME, SINGLETON_INSTANCE_FIELD, SINGLETON_LAZY_ACCESSOR, SINGLETON_UNLOAD_ACCESSOR,
        SingletonInstancePlan, StorageKind, SumTypeLayout, SumVariantLayout,
    },
    linked::{
        CanonicalCallEdge, CanonicalExternalCallEdge, CanonicalFragment, CanonicalPrimitiveType, CanonicalTypeKind, EntryRecord,
        EvidenceRecord, ExportRecord, FieldRecord, ImportRecord, ItemInstanceRecord, LinkedSemanticProgram, NominalInstanceRecord,
        NominalValueSemantics, TypeRecord, VariantRecord,
    },
    neutral_contract::{
        ArtifactContract, BootstrapStage, EvidencePackage, EvidenceStatus, PrimitiveDefinition, PrimitiveRegistry, Provenance,
        SemanticObservation, SemanticPackageInterface,
    },
    registries::{AttributeRegistry, AttributeRegistryError},
    semantic_ids::{
        AttributeId, AttributeRegistration, EffectEdgeId, EffectSiteId, EvidenceId, FieldId, GenericFunctionId, IdKind, ImportCapability,
        ImportIndex, InstructionId, ItemId, ItemInstanceId, MirValueDefinition, MirValueId, NominalInstanceId, ProvenanceId, SemanticId,
        TypeId, TypeInstanceId, ValueIdentity, VariantId, builtin_attribute,
    },
    source::{Position, SourceID, SourceSpan},
    symbols::{Identifier, NamePath, QualifiedName, SymbolIdentity},
    ty::{FunctionType, SemanticType, WitnessObject},
    witness_submission::{WitnessCallEdge, WitnessMethodSlotSubmission, WitnessSubmission},
};
pub use core_surface::{CoreFeature, CoreSurfaceManifest};

/// Compiler 输入与声明索引的中立数据结构（装配层与编译阶段之间的数据传输合同）。
pub mod compiler_input;
/// 产物 / cache 合同版本（identity / MIR / layout）。
pub mod contract_versions;
pub mod core_surface;
/// 跨阶段结构化诊断合同（语言无关）。
pub mod diagnostics;
pub mod digest;
mod errors;
mod external_import;
/// 聚合 / singleton 布局合同，供可执行 lowering 使用。
pub mod layout;
/// 链接完成的语义程序与稳定身份记录（中立事实，不含语言 IR）。
pub mod linked;
/// 目标命名投影的共享纯函数（camelCase 等大小写规则的唯一实现）。
pub mod naming;
/// 前端、规划器、emitter 与运行时共享的中立可审计合同。
pub mod neutral_contract;
/// 可扩展属性 / 运算符注册表。
pub mod registries;
/// 参数化 MIR 语义身份与稀疏 RepresentationPlan。
pub mod semantic_ids;
mod source;
mod symbols;
mod ty;
mod witness_submission;

/// 分析器、规划器与后端共享的稳定能力标签。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CapabilityTag(String);

impl CapabilityTag {
    /// 新建能力标签。
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// 返回标签字符串切片。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for CapabilityTag {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for CapabilityTag {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

/// 引用语义对象的管理策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ReferenceManagement {
    /// 交给精准式托管 GC 管理。
    HostGc,
    /// 交给 Perceus RC 管理。
    PerceusRc,
}

/// `Futamura projection` 目标家族。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum FutamuraProjectionFamily {
    /// `futa_clr`
    Clr,
    /// `futa_jvm`
    Jvm,
    /// `futa_wasm`
    Wasm,
    /// `futa_machine_code`
    MachineCode,
    /// `futa_vm`
    Ovm,
    /// `futa_gpu`
    Gpu,
}

impl FutamuraProjectionFamily {
    /// 返回稳定家族名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clr => "futa_clr",
            Self::Jvm => "futa_jvm",
            Self::Wasm => "futa_wasm",
            Self::MachineCode => "futa_machine_code",
            Self::Ovm => "futa_vm",
            Self::Gpu => "futa_gpu",
        }
    }
}

/// 目标宿主边界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum HostProjectionBoundary {
    /// `.NET` 宿主边界。
    Clr,
    /// `JVM` 宿主边界。
    Jvm,
    /// `WASM + JS glue` 宿主边界。
    WasmJsGlue,
    /// `WASI component model` 宿主边界。
    WasiComponent,
    /// 机器码宿主边界：OS 直接装载 `PE` / `ELF`，宿主包装由产物合同决定。
    MachineCode,
    /// 字节码虚拟机宿主边界。
    Vm,
}

/// 入口返回值在宿主上的可观测投影（结果通道 + 值宽 + 解码规则）。
///
/// 该事实由**实际生成入口包装的目标后端**逐条冻结进运行合同（`DriverRunContract`），
/// Legion 与运行器只消费合同里的投影，不得按 target 名字或宿主边界另行推导：
/// 同一宿主边界上不同入口包装完全可能选择不同通道（例如 JVM launcher 当前打印
/// stdout 只是该包装的选择，不能推广为 JVM 所有入口的结果通道）。
/// 合同缺失该事实时 fail-closed，不得假定默认通道。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum EntryReturnProjection {
    /// 入口包装把 i32 返回值以十进制行打印到 stdout，自身以 0 退出。
    StdoutInteger,
    /// 入口返回值就是进程退出码（按宿主约定截断到低 8 位）。
    ProcessExitCode,
    /// 宿主在进程内调用入口，返回值是 VM 内的 i32 值。
    InProcessValue,
    /// 入口没有返回值可观测。
    NoReturnValue,
}

/// `Futamura projection` 选择策略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionPolicy {
    /// 目标投影家族。
    pub family: FutamuraProjectionFamily,
    /// 宿主边界。
    pub host_boundary: HostProjectionBoundary,
    /// 引用对象管理策略。
    pub reference_management: ReferenceManagement,
    /// 是否优先缩小产物体积。
    pub prefer_small_artifacts: bool,
    /// 是否保留显式 effect 边界。
    pub preserve_effect_boundaries: bool,
}

/// 单次目标投影计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionPlan {
    /// 选中的目标家族。
    pub family: FutamuraProjectionFamily,
    /// 选中的宿主边界。
    pub host_boundary: HostProjectionBoundary,
    /// 选中的引用对象管理策略。
    pub reference_management: ReferenceManagement,
    /// 投影后仍需要保留的能力。
    pub preserved_capabilities: Vec<CapabilityTag>,
}

/// 编译器确定的运行时合同需求。
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RuntimeRequirement {
    /// 需求键。
    pub key: String,
    /// 需求值。
    pub value: String,
}

impl std::fmt::Display for CapabilityTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
