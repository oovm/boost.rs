//! 从唯一 `CompiledValkyrieProgram` 选择分区并交给目标适配器。
//!
//! 分区选择不重新生产语义事实；Valkyrie 通过语言 × 目标适配器准备目标输入，
//! 编排层只核对分区身份，不要求任何公共 MIR 作为成功载荷。
use compiler_emitter::{
    AdaptedFragmentInput, AssembledFragment, FragmentSubmission, LoweredBackendInput, TargetInputRequest,
};
use compiler_planning::{ArtifactPartition, ArtifactPartitionPlan, PlanningError};
use compiler_target::{CanonicalTarget, ClrSuspendStrategy, Identifier, projection_policy_for_target_profile};

use super::pipeline::{lane, machine};
#[cfg(feature = "ovm-lane")]
use super::pipeline::ovm;
use super::pipeline::target_plan::{self, TargetPlanError};
use crate::compiler::valkyrie_mir::CompiledValkyrieProgram;

/// 装配边界失败：身份核对失败保留一句说明，适配器失败保留结构化的
/// `TargetPlanError`（stage/instance/symbol/reason），下游不得再拍平为裸文本。
#[derive(Debug)]
pub enum AssemblyError {
    /// 分区/程序身份不一致。
    Identity(String),
    /// 语言 × 目标适配失败。
    TargetPlan {
        /// 出错的分区名。
        partition: String,
        /// 适配器产生的结构化错误。
        source: TargetPlanError,
    },
}

impl std::fmt::Display for AssemblyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Identity(message) => f.write_str(message),
            Self::TargetPlan { partition, source } => write!(f, "分区 `{partition}` 目标计划构建失败: {source}"),
        }
    }
}

impl std::error::Error for AssemblyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Identity(_) => None,
            Self::TargetPlan { source, .. } => Some(source),
        }
    }
}

/// 返回已解析的导出/入口数量；装配器不直接读取语义计划。
#[doc(hidden)]
pub fn build_output_surface_counts(compiled_program: &CompiledValkyrieProgram) -> (usize, usize) {
    let linked = &compiled_program.mir_program().linked;
    (linked.exports.len(), linked.entries.len())
}

/// 根据完整程序、目标与 projection 合同规划产物分区。
#[doc(hidden)]
pub fn plan_artifacts_from_compiled_program(
    compiled_program: &CompiledValkyrieProgram,
    target: CanonicalTarget,
    clr_suspend_strategy: ClrSuspendStrategy,
) -> std::result::Result<ArtifactPartitionPlan, PlanningError> {
    let target_profile = target.to_profile(None);
    let projection_policy = projection_policy_for_target_profile(&target_profile).map_err(|error| PlanningError::SemanticContract {
        module: compiled_program.mir_program().linked.module_name.clone(),
        detail: format!("目标 projection 合同失败: {error:?}"),
    })?;
    let linked = &compiled_program.mir_program().linked;
    let backend_registry = compiler_emitter::bundled_backend_registry_from_canonical(&linked.fragments, &target_profile, &projection_policy);
    ArtifactPartitionPlan::from_linked_program(
        linked,
        target,
        compiler_optimize::RewriteTheory::default(),
        projection_policy,
        backend_registry,
        clr_suspend_strategy,
    )
}

/// Valkyrie × 目标适配器：把已验证的 Valkyrie 程序映射为平台输入。
///
/// 平台身份与产物/宿主合同只从 `TargetInputRequest::resolved` 进入；适配器
/// 不再单独保存四元组，合同与平台不会在这里分叉。
///
/// 适配器只借用前端已编译程序与分区身份：**不**在装配阶段预构造跨目标计划。
/// 目标计划在 [``AdaptedFragmentInput::prepare``] 收到冻结目标之后才按
/// `request.resolved` 构造，因此不存在「拿到 `TargetInputRequest` 之前就已有
/// 一份共享可执行程序」的路径。
struct ValkyrieTargetAdapter<'a> {
    partition: ArtifactPartition,
    compiled_program: &'a CompiledValkyrieProgram,
    roots: Vec<compiler_types::ItemInstanceId>,
    theory_bundle: compiler_optimize::TheoryBundle,
}

impl AdaptedFragmentInput for ValkyrieTargetAdapter<'_> {
    fn partition(&self) -> &ArtifactPartition {
        &self.partition
    }

    fn prepare(&self, request: &TargetInputRequest<'_>) -> compiler_emitter::Result<LoweredBackendInput> {
        if request.backend_family == compiler_target::TargetBackendFamily::MachineCode {
            // 机器码 lane 直接消费语言 MIR：不再构造 `FragmentSubmission` /
            // `TargetExecutionPlan`。纵链两段式不变——语言侧产出目标私有输入，
            // emitter 装配服务只做编码 + 容器 + 自检，不自行拼接机器码。
            let ctx = lane::build_lane_context(self.compiled_program, &self.partition.fragment, &self.roots)
                .map_err(|error| format!("分区 `{}` 目标计划构建失败: {error}", self.partition.name))?;
            if request.artifact_purpose == compiler_target::ArtifactPurpose::Library {
                // 普通共享库：导出面就是交付合同，没有进程入口，也不绑定宿主模块。
                let library_input = machine::library::build_library_input(&ctx, &request.resolved)?;
                let lowered = compiler_emitter::emitter_backend_machine::assemble_x64_library_image(&library_input)?;
                return Ok(LoweredBackendInput::machine(compiler_emitter::emitter_backend_machine::MachineImageBackendInput {
                    executable: lowered.executable,
                    output_dir: request.output_dir.to_path_buf(),
                    // 普通共享库没有单一入口：交付合同是导出索引，不拿首导出冒充入口。
                    entry_symbol: None,
                    artifact_extension: Some(lowered.artifact_extension),
                    host_exports: lowered
                        .exports
                        .into_iter()
                        .map(|export| compiler_emitter::emitter_backend_machine::MachineHostExport {
                            source_name: export.source_name,
                            public_name: export.public_name,
                            parameters: export.parameters,
                            parameter_names: export.parameter_names,
                            returns: export.returns,
                        })
                        .collect(),
                    host_module: None,
                }));
            }
            if request.artifact_purpose == compiler_target::ArtifactPurpose::HostExtension {
                let product = compiler_emitter::emitter_backend_machine::X64ProductTarget::from_resolved(&request.resolved)
                    .map_err(|error| format!("宿主扩展目标不合法：{error}"))?;
                let extension_input = machine::extension::build_extension_input(&ctx, &request.resolved)?;
                let lowered = compiler_emitter::emitter_backend_machine::assemble_x64_extension_image(&extension_input)?;
                return Ok(LoweredBackendInput::machine(compiler_emitter::emitter_backend_machine::MachineImageBackendInput {
                    executable: lowered.executable,
                    output_dir: request.output_dir.to_path_buf(),
                    entry_symbol: Some(lowered.entry_symbol),
                    artifact_extension: Some(lowered.artifact_extension),
                    host_exports: lowered.host_exports,
                    host_module: product.host_module(),
                })
                // CPython 产物基名取归一化模块名：文件名、`PyInit_<module>`、
                // 清单与宿主 `import` 同值；工程名本身保持不变。
                .with_artifact_base_name(lowered.module_name));
            }
            // 原生可执行交付：先降成目标私有 x86-64 输入，再交装配服务。
            let program_input = machine::exe::build_program_input(&ctx, &request.resolved, request.artifact_purpose)?;
            let entry = ctx
                .fragment()
                .entry_operation
                .and_then(|instance| lane::entry_symbol(&ctx, &instance))
                .ok_or_else(|| "原生可执行交付缺少冻结的入口身份：计划未给出入口操作的 ABI 名".to_string())?;
            let lowered = compiler_emitter::emitter_backend_machine::assemble_x64_program_image(&program_input)?;
            return Ok(LoweredBackendInput::machine(compiler_emitter::emitter_backend_machine::MachineImageBackendInput {
                executable: lowered.executable,
                output_dir: request.output_dir.to_path_buf(),
                entry_symbol: Some(entry),
                artifact_extension: None,
                host_exports: Vec::new(),
                host_module: None,
            }));
        }
        #[cfg(feature = "ovm-lane")]
        if request.backend_family == compiler_target::TargetBackendFamily::Ovm {
            // OVM lane 直接消费语言 MIR：不再构造 `FragmentSubmission` /
            // `TargetExecutionPlan`。纵链两段式不变——语言侧产出目标私有输入，
            // emitter 装配服务只做模块形状自检与容器编码。
            let ctx = lane::build_lane_context(self.compiled_program, &self.partition.fragment, &self.roots)
                .map_err(|error| format!("分区 `{}` 目标计划构建失败: {error}", self.partition.name))?;
            let program_input = ovm::build_ovm_program_input(&ctx)
                .map_err(|error| format!("分区 `{}` 目标计划构建失败: {error}", self.partition.name))?;
            let (ovm_module, abi_contract) = compiler_emitter::emitter_backend_vm::assemble_ovm_module(&program_input)?;
            return Ok(LoweredBackendInput::ovm(compiler_emitter::OvmBackendInput {
                ovm_module: Some(ovm_module),
                abi_contract,
                library_public_exports: ovm::library_public_export_names(&ctx),
                output_dir: request.output_dir.to_path_buf(),
            })
            // 产物基名沿用旧公共路径的合同：入口操作 ABI 名末段清洗；无入口时
            // 回落工程名（`with_artifact_base_name(None)` 不覆盖）。
            .with_artifact_base_name(entry_artifact_name(&ctx)));
        }
        let plan = target_plan::from_compiled_program(
            self.compiled_program,
            &self.partition.fragment,
            self.theory_bundle.clone(),
            &self.roots,
        )
        .map_err(|error| format!("分区 `{}` 目标计划构建失败: {error}", self.partition.name))?;
        let submission = FragmentSubmission::from_backend_plan(plan);
        LoweredBackendInput::from_fragment_submission(
            &submission,
            request.backend_family,
            request.host_boundary,
            request.output_dir,
            request.host_flavor,
            request.artifact_purpose,
            request.wasm_package_kind,
        )
    }
}

/// 入口产物基名：入口操作 ABI 名末段清洗（与旧公共路径 `entry_artifact_name` 同合同）。
#[cfg(feature = "ovm-lane")]
fn entry_artifact_name(ctx: &lane::LaneContext) -> Option<String> {
    let name = ctx.fragment().entry_operation.and_then(|instance| ctx.abi_name(&instance))?;
    let tail = name.parts().last()?.as_str();
    let sanitized = tail.chars().map(|ch| if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' { ch } else { '_' }).collect::<String>();
    (!sanitized.is_empty()).then_some(sanitized)
}

/// 从指定分区装配目标载荷，拒绝任何与链接身份不一致的计划。
#[doc(hidden)]
pub fn assemble_fragment<'a>(
    compiled_program: &'a CompiledValkyrieProgram,
    plan: &ArtifactPartitionPlan,
    partition_index: usize,
) -> std::result::Result<AssembledFragment<'a>, AssemblyError> {
    if plan.module_name.to_string() != compiled_program.mir_program().linked.module_name {
        return Err(AssemblyError::Identity(format!("前端计划与分区计划不匹配")));
    }
    if partition_index >= plan.partitions.len() {
        return Err(AssemblyError::Identity(format!("分区索引 `{partition_index}` 超出范围")));
    }
    let partition = &plan.partitions[partition_index];
    let fragment_view = plan
        .fragment_views
        .iter()
        .find(|view| view.fragment_id == partition.fragment)
        .ok_or_else(|| AssemblyError::Identity(format!("分区 `{}` 缺少优化后的语义片段视图", partition.name)))?;
    let linked = &compiled_program.mir_program().linked;
    let fragment = linked
        .fragments
        .get(&partition.fragment)
        .ok_or_else(|| AssemblyError::Identity(format!("分区 `{}` 对应的语义片段不存在", partition.name)))?;
    if partition.exported_operations != fragment.exported_operations
        || partition.entry_operation != fragment.entry_operation
        || fragment_view.canonical_operations != fragment.exported_operations
    {
        return Err(AssemblyError::Identity(format!("分区 `{}` 的 callable 身份与片段合同不一致", partition.name)));
    }

    let fragment_requires_suspend = fragment.required_capabilities.iter().any(|capability| capability.as_str() == "suspend");
    if fragment_requires_suspend {
        return Err(AssemblyError::Identity(format!("分区 `{}` 要求 suspend，但 Compiler 尚未提供已验证的 suspend 合同", partition.name)));
    }

    let mut roots = partition.exported_operations.clone();
    if let Some(entry) = partition.entry_operation
        && !roots.contains(&entry)
    {
        roots.push(entry);
    }
    let theory_bundle = fragment_view.theory_bundle.clone();

    // 目标计划不在此处构造：适配器只持有已编译程序与分区身份，等编排层交出
    // 冻结目标后在 `prepare` 内按 `request.resolved` 构造目标输入。
    // sum tag 的物理宽度是各 lane 自己的事实，不由编排层按 backend family
    // 预先取好传给语言适配器。
    Ok(AssembledFragment {
        partition: partition.clone(),
        theory_bundle: theory_bundle.clone(),
        input: Box::new(ValkyrieTargetAdapter { partition: partition.clone(), compiled_program, roots, theory_bundle }),
    })
}
