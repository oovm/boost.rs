#![doc = include_str!("readme.md")]

pub mod control_flow;
pub mod rewrite_theory_manifest;

pub use rewrite_theory_manifest::{
    RewriteTheoryEquationEntryV1, RewriteTheoryManifestV1, RewriteTheoryRuleEntryV1, RewriteTheoryTermRewriteEntryV1, builtin_graphic_manifest,
    builtin_neural_manifest,
};

pub use control_flow::{
    ControlFlowPayload, SuspendContinuationArtifact, SuspendDispatchCase, SuspendFunctionArtifact, SuspendRuntimeFunctionArtifact,
    SuspendRuntimePayload, SuspendStateArtifact, SuspendWitnessBinding,
};

use std::collections::BTreeMap;

use compiler_optimize::{RewriteTheory, TheoryBundle};
use compiler_types::{
    CapabilityTag, ExportRecord, FutamuraProjectionFamily, HostProjectionBoundary, Identifier, ItemInstanceId, ProjectionPlan, ProjectionPolicy,
    QualifiedName, ReferenceManagement, RuntimeRequirement,
};

use compiler_target::{
    abstractions::{BackendInputKind, BinaryTarget, CanonicalTarget, PartitionBackendRequirement},
    backends::BackendRegistry,
    packaging::TargetLane,
};

/// 规划阶段失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanningError {
    /// Canonical 语义程序或表示计划未能建立。
    SemanticContract {
        /// 当前模块。
        module: String,
        /// 结构化诊断摘要。
        detail: String,
    },
    /// 没有找到可解释当前片段的后端解释器。
    MissingBackendInterpreter {
        /// 当前片段。
        fragment: Identifier,
        /// 当前投影家族。
        projection_family: FutamuraProjectionFamily,
        /// 当前宿主边界。
        host_boundary: HostProjectionBoundary,
    },
}

impl std::fmt::Display for PlanningError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SemanticContract { module, detail } => {
                write!(f, "planning failed for module `{module}`: {detail}")
            }
            Self::MissingBackendInterpreter { fragment, projection_family, host_boundary } => {
                write!(f, "no backend interpreter for fragment `{fragment}` (family={projection_family:?}, host={host_boundary:?})")
            }
        }
    }
}

impl std::error::Error for PlanningError {}

/// 进入目标规划层的中立输入。
#[derive(Debug, Clone, PartialEq, Eq)]
struct OptimizerInput {
    /// 逻辑模块名。
    pub module_name: QualifiedName,
    /// 目标。
    pub target: CanonicalTarget,
    /// 下游已经闭合好的程序事实。
    pub program_facts: PlannerFacts,
    /// 前端提交的语义片段标识（名称边界已在入口校验通过）。
    ///
    /// 这些片段只描述“有哪些可组合的语义视图”，不承担 callable、入口或导入
    /// 绑定，也不把整个程序压平为一个单体 `IR`。
    pub semantic_fragments: Vec<Identifier>,
    /// 当前启用的等价理论。
    pub rewrite_theory: RewriteTheory,
    /// 目标投影策略。
    pub projection_policy: ProjectionPolicy,
    /// 面向当前构建请求的后端解释器注册表。
    pub backend_registry: BackendRegistry,
    /// CLR suspend 策略（非 CLR target 时忽略）。
    pub clr_suspend_strategy: compiler_target::backends::clr::ClrSuspendStrategy,
}

/// Compiler 从 HIR 声明节点直接绑定的片段根合同。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticFragment {
    /// 片段标识。
    pub id: Identifier,
    /// 已实例化的片段根，不包含名称重建的调用闭包。
    pub exported_operations: Vec<compiler_types::ItemInstanceId>,
    /// 激活片段所需的能力。
    pub required_capabilities: Vec<CapabilityTag>,
    /// 片段引用管理提示。
    pub reference_management_hint: Option<ReferenceManagement>,
    /// 声明绑定的入口实例。
    pub entry_operation: Option<compiler_types::ItemInstanceId>,
    /// 片段等式理论。
    pub rewrite_theory: RewriteTheory,
    /// 实例到语言侧冻结源码名的映射。
    pub export_names: BTreeMap<compiler_types::ItemInstanceId, String>,
    /// 实例到 `[export(name:)]` / `[export(case:)]` 注解解析名的映射；
    /// 只有显式注解改名的实例才有 entry，由各目标所有者按合同投影。
    pub export_annotations: BTreeMap<compiler_types::ItemInstanceId, String>,
}

/// 从 Canonical 闭包派生的分区事实视图，不是前端成功合同。
///
/// 导出合同只用于入口处的 fail-closed 身份校验（每个导出 callable 必须有冻结
/// 限定名），不进分区计划。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct PlannerFacts {
    capabilities: Vec<CapabilityTag>,
    reference_management: Option<ReferenceManagement>,
    runtime_requirements: Vec<RuntimeRequirement>,
}

/// 单个分区计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactPartition {
    /// 当前分区对应的语义片段。
    pub fragment: Identifier,
    /// 当前片段的解释入口。
    pub entry_operation: Option<compiler_types::ItemInstanceId>,
    /// 选中的后端名。
    pub backend_name: String,
    /// 选中的解释器名。
    pub interpreter: Identifier,
    /// 分区逻辑名。
    pub name: String,
    /// 当前分区对外暴露的操作。
    pub exported_operations: Vec<compiler_types::ItemInstanceId>,
    /// 目标路线。
    pub lane: TargetLane,
    /// 面向的二进制目标。
    pub binary_target: BinaryTarget,
    /// 预期 backend 输入。
    pub input_kind: Option<BackendInputKind>,
    /// CLR suspend 策略（仅 `lane == Clr` 时有效）。
    pub clr_suspend_strategy: compiler_target::backends::clr::ClrSuspendStrategy,
    /// 宿主边界。
    pub host_boundary: HostProjectionBoundary,
    /// 引用对象管理策略。
    pub reference_management: ReferenceManagement,
    /// 本分区要求的能力标签。
    pub capabilities: Vec<CapabilityTag>,
    /// 本分区要求的运行时契约。
    pub runtime_requirements: Vec<RuntimeRequirement>,
}

/// 单个语义片段的优化视图。
///
/// 只携带分区与装配实际消费的事实：canonical 操作身份、已采用的规则名
/// 与理论 bundle。等式优化不再经由伪 `ObjectAlgebraicProgram` 往返。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FragmentOptimizationView {
    /// 片段标识。
    pub fragment_id: Identifier,
    /// Canonical 合同确定的稳定操作；等式优化不得重绑定函数身份。
    pub canonical_operations: Vec<compiler_types::ItemInstanceId>,
    /// 本轮采用的规则名。
    pub applied_rules: Vec<Identifier>,
    /// 共享理论 + 片段理论合并后的 bundle。
    pub theory_bundle: TheoryBundle,
}

/// 中性的产物分区计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactPartitionPlan {
    /// 逻辑模块名。
    pub module_name: QualifiedName,
    /// 目标。
    pub target: CanonicalTarget,
    /// 分区列表。
    pub partitions: Vec<ArtifactPartition>,
    /// 按片段暴露的优化视图。
    pub fragment_views: Vec<FragmentOptimizationView>,
}

/// 分区主路径实际消费的单个片段窄事实。
///
/// 分区链只消费片段身份、公开操作的 callable 身份、要求能力与入口身份；
/// 导入候选、调用边与导出注解是语言与各目标 lane 的事实，不进规划输入。
#[derive(Debug)]
pub struct PlanningFragmentFacts<'a> {
    /// 片段身份。
    pub id: &'a Identifier,
    /// 片段公开及闭包内操作的 callable 身份。
    pub exported_operations: &'a [ItemInstanceId],
    /// 片段要求的能力。
    pub required_capabilities: &'a [CapabilityTag],
    /// 片段入口 callable 身份。
    pub entry_operation: Option<ItemInstanceId>,
}

/// 分区规划入口的窄视图：只携带规划实际消费的有限事实。
///
/// 规划层不再接收整份 `LinkedSemanticProgram`，也不再借用整张 `CanonicalFragment`
/// 表——证据、字段、变体、数组索引类型与布局表都不进规划输入；
/// 这些是语言与各目标 lane 的事实，不是分区规划的事实。
#[derive(Debug)]
pub struct PlanningProgramView<'a> {
    /// 模块 / 包身份。
    pub module_name: &'a str,
    /// 已结算的语义片段窄事实表（顺序即分区顺序，由语言前端按稳定身份排好）。
    pub fragments: &'a [PlanningFragmentFacts<'a>],
    /// callable 身份到限定 ABI 名称的冻结映射。
    pub callable_names: &'a BTreeMap<ItemInstanceId, QualifiedName>,
    /// 公开导出合同（身份 → 源名）。
    pub exports: &'a BTreeMap<ItemInstanceId, ExportRecord>,
    /// 模块级引用管理合同。
    pub reference_management: Option<ReferenceManagement>,
    /// 模块级运行时合同需求。
    pub runtime_requirements: &'a [RuntimeRequirement],
}

impl ArtifactPartitionPlan {
    /// 从语言前端已验证并提交的窄规划视图派生分区计划。
    ///
    /// 编排层不解释语言语义，也不持有语言 MIR：语言前端必须先完成自己的语义
    /// 验证，再把身份/片段/调用/运行需求这些有限事实交给这里。可执行入口
    /// 与根直接保留已冻结的 callable 身份；optimizer 名称视图只用于等式优化，
    /// 不得从 HIR 或旧前端计划重新收集绑定事实。
    pub fn from_program_view(
        view: PlanningProgramView<'_>,
        target: CanonicalTarget,
        rewrite_theory: RewriteTheory,
        projection_policy: ProjectionPolicy,
        backend_registry: BackendRegistry,
        clr_suspend_strategy: compiler_target::backends::clr::ClrSuspendStrategy,
    ) -> Result<Self, PlanningError> {
        let module_name = qualified_module_name(view.module_name);
        let (program_facts, semantic_fragments) = canonical_optimizer_input(&view)?;
        Self::plan_from_optimizer_input(
            OptimizerInput {
                module_name,
                target,
                program_facts,
                semantic_fragments,
                rewrite_theory,
                projection_policy,
                backend_registry,
                clr_suspend_strategy,
            },
            view.fragments,
        )
    }

    /// 基于程序事实生成最小分区计划；分区器直接消费片段窄事实。
    fn plan_from_optimizer_input(input: OptimizerInput, fragments: &[PlanningFragmentFacts<'_>]) -> Result<Self, PlanningError> {
        let OptimizerInput {
            module_name,
            target,
            program_facts,
            semantic_fragments,
            rewrite_theory,
            projection_policy,
            backend_registry,
            clr_suspend_strategy,
        } = input;
        if semantic_fragments.is_empty() {
            return Err(PlanningError::SemanticContract {
                module: module_name.to_string(),
                detail: "缺少 Compiler 已验证的 semantic fragment；禁止回退到 ObjectAlgebraicProgram".to_owned(),
            });
        }
        let mut projection_policy = projection_policy;
        projection_policy.reference_management =
            resolve_reference_management(program_facts.reference_management, projection_policy.reference_management, projection_policy.family);
        // 分区链只消费投影元数据与能力事实：投影计划由策略与片段事实直接结算，
        // 不再合成 `structured_terms` 为空的伪 `ObjectAlgebraicProgram` 走 optimizer 往返。
        let preserved_capabilities = program_facts
            .capabilities
            .iter()
            .filter(|capability| projection_policy.preserve_effect_boundaries || capability.as_str() != "suspend")
            .cloned()
            .collect::<Vec<_>>();
        let projection = ProjectionPlan {
            family: projection_policy.family,
            host_boundary: projection_policy.host_boundary,
            reference_management: projection_policy.reference_management,
            preserved_capabilities,
        };
        let applied_rules = rewrite_theory
            .rules
            .iter()
            .filter(|rule| rule.allowed_projection_families.is_empty() || rule.allowed_projection_families.contains(&projection_policy.family))
            .filter(|rule| rule.required_capabilities.iter().all(|capability| program_facts.capabilities.iter().any(|provided| provided == capability)))
            .map(|rule| rule.name.clone())
            .collect::<Vec<_>>();
        let fragment_views = build_fragment_views(fragments, &applied_rules, &rewrite_theory);
        let binary_target: BinaryTarget = target.into();
        let partitions =
            build_partitions(&program_facts, &projection, &module_name, fragments, binary_target, &backend_registry, clr_suspend_strategy)?;
        Ok(Self { module_name, target, partitions, fragment_views })
    }

    /// 从指定分区提取已经完成规划的后端需求。
    pub fn backend_requirement(&self, partition_index: usize) -> Option<PartitionBackendRequirement> {
        let partition = self.partitions.get(partition_index)?;
        Some(PartitionBackendRequirement {
            backend_name: partition.backend_name.clone(),
            interpreter: partition.interpreter.clone(),
            fragment: partition.fragment.clone(),
            lane: partition.lane,
            input_kind: partition.input_kind?,
            target: partition.binary_target.clone(),
            host_boundary: partition.host_boundary,
            reference_management: partition.reference_management,
        })
    }
}

fn qualified_module_name(module_name: &str) -> QualifiedName {
    QualifiedName::new(module_name.split("::").filter(|part| !part.is_empty()).map(Identifier::new).collect())
}

fn canonical_optimizer_input(view: &PlanningProgramView<'_>) -> Result<(PlannerFacts, Vec<Identifier>), PlanningError> {
    let callable_name = |instance: ItemInstanceId| {
        view.callable_names.get(&instance).cloned().ok_or_else(|| PlanningError::SemanticContract {
            module: view.module_name.to_string(),
            detail: format!("Canonical callable `{instance:?}` 缺少限定名称"),
        })
    };
    let mut capabilities = Vec::new();
    let mut fragments = Vec::new();
    for fragment in view.fragments {
        for capability in fragment.required_capabilities {
            if !capabilities.contains(capability) {
                capabilities.push(capability.clone());
            }
        }
        // fail-closed 身份校验：片段公开操作必须有冻结限定名；名称本身不进计划。
        for instance in fragment.exported_operations {
            callable_name(*instance)?;
        }
        fragments.push(fragment.id.clone());
    }
    // fail-closed 身份校验：每个导出 callable 必须有冻结限定名；导出表本身不进分区计划。
    for instance in view.exports.keys() {
        callable_name(*instance)?;
    }
    let program_facts = PlannerFacts {
        capabilities,
        reference_management: view.reference_management,
        runtime_requirements: view.runtime_requirements.to_vec(),
    };
    Ok((program_facts, fragments))
}

fn build_fragment_views(
    fragments: &[PlanningFragmentFacts<'_>],
    applied_rules: &[Identifier],
    shared_theory: &RewriteTheory,
) -> Vec<FragmentOptimizationView> {
    fragments
        .iter()
        .map(|fragment| FragmentOptimizationView {
            fragment_id: fragment.id.clone(),
            canonical_operations: fragment.exported_operations.to_vec(),
            applied_rules: applied_rules.to_vec(),
            theory_bundle: TheoryBundle { shared: shared_theory.clone(), fragment: RewriteTheory::default() },
        })
        .collect()
}

fn resolve_reference_management(
    preferred: Option<ReferenceManagement>,
    fallback: ReferenceManagement,
    family: FutamuraProjectionFamily,
) -> ReferenceManagement {
    match (family, preferred) {
        (FutamuraProjectionFamily::MachineCode, _) => ReferenceManagement::PerceusRc,
        (_, Some(value)) => value,
        _ => fallback,
    }
}

fn resolve_partition_reference_management(
    program_facts: &PlannerFacts,
    fallback: ReferenceManagement,
    family: FutamuraProjectionFamily,
) -> ReferenceManagement {
    if family == FutamuraProjectionFamily::MachineCode {
        return ReferenceManagement::PerceusRc;
    }

    program_facts.reference_management.unwrap_or(fallback)
}

fn clr_suspend_strategy_for_lane(
    lane: TargetLane,
    strategy: compiler_target::backends::clr::ClrSuspendStrategy,
) -> compiler_target::backends::clr::ClrSuspendStrategy {
    if lane == TargetLane::Clr { strategy } else { compiler_target::backends::clr::ClrSuspendStrategy::default() }
}

fn build_partitions(
    program_facts: &PlannerFacts,
    projection: &ProjectionPlan,
    module_name: &QualifiedName,
    fragments: &[PlanningFragmentFacts<'_>],
    binary_target: BinaryTarget,
    backend_registry: &BackendRegistry,
    clr_suspend_strategy: compiler_target::backends::clr::ClrSuspendStrategy,
) -> Result<Vec<ArtifactPartition>, PlanningError> {
    fragments
        .iter()
        .map(|fragment| {
            let capabilities = resolve_partition_capabilities(&projection.preserved_capabilities, fragment.required_capabilities);
            let reference_management =
                resolve_partition_reference_management(program_facts, projection.reference_management, projection.family);
            let backend = backend_registry
                .resolve(
                    fragment.id,
                    &capabilities,
                    &binary_target,
                    projection.family,
                    projection.host_boundary,
                    reference_management,
                )
                .ok_or_else(|| PlanningError::MissingBackendInterpreter {
                    fragment: fragment.id.clone(),
                    projection_family: projection.family,
                    host_boundary: projection.host_boundary,
                })?;
            Ok(ArtifactPartition {
                fragment: fragment.id.clone(),
                entry_operation: fragment.entry_operation,
                backend_name: backend.backend_name,
                interpreter: backend.interpreter,
                name: format!("{module_name}::{}", fragment.id),
                exported_operations: fragment.exported_operations.to_vec(),
                lane: backend.lane,
                binary_target: binary_target.clone(),
                input_kind: backend.input_kind,
                clr_suspend_strategy: clr_suspend_strategy_for_lane(backend.lane, clr_suspend_strategy),
                host_boundary: backend.host_boundary,
                reference_management: backend.reference_management,
                capabilities: capabilities.clone(),
                runtime_requirements: resolve_partition_runtime_requirements(&program_facts.runtime_requirements, &capabilities),
            })
        })
        .collect()
}

fn resolve_partition_capabilities(
    preserved_capabilities: &[CapabilityTag],
    fragment_required_capabilities: &[CapabilityTag],
) -> Vec<CapabilityTag> {
    if fragment_required_capabilities.is_empty() { preserved_capabilities.to_vec() } else { fragment_required_capabilities.to_vec() }
}

fn resolve_partition_runtime_requirements(
    runtime_requirements: &[RuntimeRequirement],
    capabilities: &[CapabilityTag],
) -> Vec<RuntimeRequirement> {
    runtime_requirements
        .iter()
        .filter(|requirement| {
            requirement.key == "reference-management" || capabilities.iter().any(|capability| capability.as_str() == requirement.key)
        })
        .cloned()
        .collect()
}
