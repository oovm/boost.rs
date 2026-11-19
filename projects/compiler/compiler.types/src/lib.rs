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
