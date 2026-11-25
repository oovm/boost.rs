//! Valkyrie × 机器码目标适配：直接消费语言 MIR，产出目标私有 x86-64 输入。
//!
//! 机器码 lane 不再经 `FragmentSubmission` / `TargetExecutionPlan`：本模块读取
//! 已验证的 Valkyrie MIR 与表示计划，在语言侧完成可达闭包、类型投影、布局表与
//! 函数级 lowering，产出目标私有的 [`X64ProgramInput`] / [`X64LibraryProgramInput`]
//! / [`X64ExtensionProgramInput`]，再交 emitter 的装配服务编码为镜像。
//! JVM/CLR/Wasm/OVM 仍走 `target_plan` 的公共 IR 路径，与本模块无关。

pub(crate) mod exe;
pub(crate) mod extension;
pub(crate) mod library;
pub(crate) mod lowerer;

use std::collections::{BTreeMap, BTreeSet};

use compiler_emitter::{BackendImport, SumRepresentation};
use compiler_types::{
    CanonicalTypeKind, ItemInstanceId, QualifiedName, SemanticType, SumTypeLayout, ValueIdentity,
    semantic_ids::{ImportIndex, MirValueId, NominalInstanceId, TypeId, VariantId},
};

use super::super::super::valkyrie_mir::{
    CompiledValkyrieProgram, InvokeLowering, ValkyrieMirCallee, ValkyrieMirEffectKind, ValkyrieMirFunction, ValkyrieMirOperation,
    ValkyrieMirProgram, ValkyrieMirTerminator, ValueRepresentation,
};
use super::target_plan::{self, AdapterStage, TargetPlanError};

/// 机器码适配的失败闭合：链接事实与语言合同无法无损投影时按阶段归因。
fn unsound(stage: AdapterStage, instance: Option<ItemInstanceId>, symbol: Option<String>, reason: String) -> TargetPlanError {
    TargetPlanError::Unsound { stage, instance, symbol, reason }
}

/// 一个可达函数实例在机器码 lane 的预解析视图：语义类型与声明名已冻结，
/// lowering 期不再重复投影 `TypeId`。
pub(crate) struct MachineFunctionView {
    /// ABI 符号名（错误文案的 function 上下文）。
    pub(crate) symbol: String,
    /// 形参语义类型，按声明顺序。
    pub(crate) param_types: Vec<SemanticType>,
    /// 返回语义类型。
    pub(crate) return_type: SemanticType,
    /// SSA 值 → 语义类型（含块参数与指令结果）。
    pub(crate) value_types: BTreeMap<MirValueId, SemanticType>,
    /// canonical 冻结的声明参数名，按位置。
    pub(crate) parameter_names: Vec<String>,
}

/// 机器码 lane 的共享上下文：一次构建，三个交付入口复用。
pub(crate) struct MachineContext<'a> {
    /// 链接后的 canonical 程序（类型、布局、导入与宿主事实的唯一来源）。
    canonical: &'a ValkyrieMirProgram,
    /// 交付片段身份：导出面、注解与入口操作。
    fragment: &'a compiler_types::CanonicalFragment,
    /// 机器码可达闭包：导出 + 入口为根，沿 `Invoke{Item}` 边按最小未访问优先遍历。
    reachable: Vec<ItemInstanceId>,
    /// 闭包内函数实例的 ABI 名；导入实例不在此表（旧路径同样不给导入登记符号）。
    abi_names: BTreeMap<ItemInstanceId, QualifiedName>,
    /// 闭包内已解析的宿主导入（operator 调用合同）。
    imports: BTreeMap<ItemInstanceId, BackendImport>,
    /// 闭包内函数实例的预解析视图。
    views: BTreeMap<ItemInstanceId, MachineFunctionView>,
    /// 语义类型 → sum 布局（unite 表示）。
    sum_layout_by_semantic_type: BTreeMap<SemanticType, SumTypeLayout>,
    /// sum 实例的冻结 tag/payload 表示。
    sum_reps: BTreeMap<NominalInstanceId, SumRepresentation>,
}

impl MachineContext<'_> {
    /// 链接事实：类型登记、聚合布局计划与按字段/按类型的布局绑定。
    pub(crate) fn linked(&self) -> &compiler_types::LinkedSemanticProgram {
        &self.canonical.linked
    }

    /// 交付片段的导出面、注解与入口操作。
    pub(crate) fn fragment(&self) -> &compiler_types::CanonicalFragment {
        self.fragment
    }

    /// 可达闭包（镜像符号布局的发射顺序，必须与旧路径一致）。
    pub(crate) fn reachable(&self) -> &[ItemInstanceId] {
        &self.reachable
    }

    /// 已解析宿主导入（operator 调用合同）。
    pub(crate) fn imports(&self) -> &BTreeMap<ItemInstanceId, BackendImport> {
        &self.imports
    }

    /// 闭包内函数实例的 ABI 名（导入实例无登记，与旧路径一致）。
    pub(crate) fn abi_name(&self, instance: &ItemInstanceId) -> Option<&QualifiedName> {
        self.abi_names.get(instance)
    }

    /// 函数实例的预解析视图。
    pub(crate) fn view(&self, instance: &ItemInstanceId) -> Option<&MachineFunctionView> {
        self.views.get(instance)
    }

    /// MIR 函数体。
    pub(crate) fn function(&self, instance: &ItemInstanceId) -> Option<&ValkyrieMirFunction> {
        self.canonical.mir.functions.get(instance)
    }

    /// 语义类型 → sum 布局。
    pub(crate) fn sum_layout_by_semantic_type(&self) -> &BTreeMap<SemanticType, SumTypeLayout> {
        &self.sum_layout_by_semantic_type
    }

    /// sum 实例冻结表示。
    pub(crate) fn sum_representations(&self) -> &BTreeMap<NominalInstanceId, SumRepresentation> {
        &self.sum_reps
    }

    /// canonical 类型投影（指令携带 `TypeId` 的按需解析）。
    pub(crate) fn lower_type(&self, id: TypeId) -> Result<SemanticType, String> {
        target_plan::lower_type(self.canonical, id)
    }
}

/// 从已编译程序构建机器码 lane 上下文：片段/根身份、导入唯一性、可达闭包、
/// 逐函数合同校验与类型投影、布局与 sum 表示表。
///
/// 该阶段对应旧 `from_compiled_program` 的链接与函数降层合同校验；失败按
/// `TargetPlanError` 结构化返回，装配层保留原有的分区前缀文案。
pub(crate) fn build_context<'a>(
    program: &'a CompiledValkyrieProgram,
    fragment_id: &compiler_target::Identifier,
    roots: &[ItemInstanceId],
) -> std::result::Result<MachineContext<'a>, TargetPlanError> {
    let canonical = program.mir_program();
    if !canonical.linked.fragments.contains_key(fragment_id) {
        return Err(TargetPlanError::UnknownFragment { fragment: fragment_id.to_string() });
    }
    for root in roots {
        if !canonical.mir.functions.contains_key(root) {
            return Err(TargetPlanError::UnknownRoot { instance: *root });
        }
    }
    let fragment = canonical
        .linked
        .fragments
        .get(fragment_id)
        .ok_or_else(|| unsound(AdapterStage::Link, None, None, format!("Canonical 片段 `{fragment_id}` 不存在")))?;
    // 导入声明唯一性：同一实例不得同时绑定函数体与导入，也不得绑定多个 ImportIndex。
    let mut declared_imports: BTreeMap<ItemInstanceId, (ImportIndex, &compiler_types::ImportRecord)> = BTreeMap::new();
    for (index, import) in &canonical.linked.imports {
        if canonical.mir.functions.contains_key(&import.callee) {
            return Err(unsound(
                AdapterStage::Link,
                Some(import.callee),
                None,
                format!("callable 实例 `{:?}` 同时绑定函数体与导入", import.callee),
            ));
        }
        if declared_imports.insert(import.callee, (*index, import)).is_some() {
            return Err(unsound(
                AdapterStage::Link,
                Some(import.callee),
                None,
                format!("callable 实例 `{:?}` 绑定多个 ImportIndex", import.callee),
            ));
        }
    }
    // 机器码可达闭包：与旧 collect_reachable_functions 相同的遍历顺序——
    // 每轮取最小未访问实例，沿 `Invoke{Item}` 且「有函数体且非导入」的边扩展。
    let mut pending_roots: BTreeSet<ItemInstanceId> = fragment
        .export_names
        .keys()
        .chain(fragment.exported_operations.iter())
        .copied()
        .collect();
    if let Some(entry) = fragment.entry_operation {
        pending_roots.insert(entry);
    }
    let mut visited = BTreeSet::new();
    let mut reachable = Vec::new();
    loop {
        let Some(instance) = pending_roots.iter().copied().find(|instance| !visited.contains(instance)) else {
            break;
        };
        visited.insert(instance);
        let Some(function) = canonical.mir.functions.get(&instance) else {
            continue;
        };
        for block in function.blocks.values() {
            for instruction in &block.instructions {
                if let ValkyrieMirOperation::Invoke { callee: ValkyrieMirCallee::Item(callee), .. } = &instruction.operation
                    && canonical.mir.functions.contains_key(callee)
                    && !declared_imports.contains_key(callee)
                {
                    pending_roots.insert(*callee);
                }
            }
        }
        reachable.push(instance);
    }
    // 闭包内逐实例：导入解析签名，函数体校验合同并预解析类型视图。
    let mut imports = BTreeMap::new();
    let mut abi_names = BTreeMap::new();
    let mut views = BTreeMap::new();
    for instance in &reachable {
        if let Some((index, import)) = declared_imports.get(instance) {
            let parameter_types = import
                .parameter_types
                .iter()
                .map(|ty| target_plan::lower_type(canonical, *ty).map_err(|reason| unsound(AdapterStage::Type, Some(*instance), None, reason)))
                .collect::<std::result::Result<Vec<_>, TargetPlanError>>()?;
            let return_type = target_plan::lower_type(canonical, import.return_type)
                .map_err(|reason| unsound(AdapterStage::Type, Some(*instance), None, reason))?;
            imports.insert(*instance, BackendImport { index: *index, links: import.links.clone(), parameter_types, return_type });
            continue;
        }
        let name = canonical.linked.callable_names.get(instance).ok_or_else(|| {
            unsound(AdapterStage::Link, Some(*instance), None, format!("callable 实例 `{instance:?}` 缺少 ABI 名称"))
        })?;
        abi_names.insert(*instance, name.clone());
        let function = canonical.mir.functions.get(instance).ok_or_else(|| {
            unsound(AdapterStage::Link, Some(*instance), Some(name.to_string()), format!("callable `{name}` 缺少 canonical 函数体"))
        })?;
        // 可达调用的目标必须已有闭合实例。签名仍含 Generic 的声明模板没有被前端证明
        // 不可达，只是从前端单态化里漏掉；把它从闭包队列里滤掉等于把这处缺口伪装成
        // 编译成功，因此这里失败闭合，要求前端提供单态化/evidence 合同。
        if target_plan::signature_contains_generic(canonical, function) {
            return Err(unsound(
                AdapterStage::Function,
                Some(*instance),
                Some(name.to_string()),
                format!(
                    "可达调用的目标缺少闭合实例: instance={instance:?} symbol={name} 的声明签名仍含 Generic 类型参数，\
                     backend 无法编码；需前端提供单态化/evidence 合同"
                ),
            ));
        }
        let view = build_function_view(program, canonical, function)
            .map_err(|reason| unsound(AdapterStage::Function, Some(*instance), Some(name.to_string()), reason))?;
        views.insert(*instance, view);
    }
    // 语义类型 → 布局身份折叠表：与旧 build_plan 相同的键归一规则。
    let mut aggregate_layout_by_semantic_type = BTreeMap::new();
    for (type_id, layout_id) in &canonical.linked.aggregate_layout_by_type {
        // lower_type 只对“可执行类型”有结果；函数签名等非值类型跳过即可，
        // 它们不会成为容器元素或导出位置。
        if let Ok(ty) = target_plan::lower_type(canonical, *type_id) {
            aggregate_layout_by_semantic_type.insert(ty.clone(), *layout_id);
            // 同一实例类型有两种冻结形态：SSA/签名已结算为携带布局身份的
            // `Nominal`，而布局计划的字段事实仍以未结算的 `Apply` 记录。
            // 两者必须指向同一布局，否则字段合同会因形态差异误判为不同类型。
            // 键按 `contract_type_key` 归一（抹掉诊断拼写），两侧同一键即同一类型。
            aggregate_layout_by_semantic_type.insert(ty.contract_key(), *layout_id);
            if let Ok(unsettled) = target_plan::lower_unsettled_contract_key(canonical, *type_id) {
                aggregate_layout_by_semantic_type.insert(unsettled, *layout_id);
            }
        }
        if let Some(encoding) = canonical.linked.text_encoding_types.get(type_id) {
            let folded = match encoding {
                compiler_types::CanonicalPrimitiveType::Utf8 => SemanticType::Utf8,
                compiler_types::CanonicalPrimitiveType::Utf16 => SemanticType::Utf16,
                other => {
                    return Err(unsound(
                        AdapterStage::Representation,
                        None,
                        None,
                        format!("文本载体 `TypeId` 登记了非文本编码身份: {other:?}"),
                    ));
                }
            };
            if let Some(existing) = aggregate_layout_by_semantic_type.insert(folded.clone(), *layout_id)
                && existing != *layout_id
            {
                return Err(unsound(
                    AdapterStage::Representation,
                    None,
                    None,
                    format!("文本编码身份 `{folded:?}` 登记了冲突的托管布局: {existing:?} vs {layout_id:?}"),
                ));
            }
        }
    }
    let mut sum_layout_by_semantic_type = BTreeMap::new();
    for sum in &canonical.linked.sum_types {
        if !sum.is_unite {
            continue;
        }
        let Some(record) = canonical.linked.nominal_instances.get(&sum.nominal) else {
            continue;
        };
        if let Ok(ty) = target_plan::lower_type(canonical, record.ty) {
            sum_layout_by_semantic_type.insert(ty, sum.clone());
        }
    }
    let sum_reps = target_plan::platform_sum_representations(canonical)
        .map_err(|reason| unsound(AdapterStage::Representation, None, None, reason))?;
    // `aggregate_layout_by_semantic_type` 只在此处完成折叠与冲突校验（机器码
    // lowering 经 `aggregate_layouts.frozen_layout_id` 直接取布局身份），不留存。
    drop(aggregate_layout_by_semantic_type);
    Ok(MachineContext {
        canonical,
        fragment,
        reachable,
        abi_names,
        imports,
        views,
        sum_layout_by_semantic_type,
        sum_reps,
    })
}

/// 函数级合同校验 + 类型预解析：值表示合同、参数名冻结、sum variant 合同、
/// invoke 降低合同、终结符支持面，最后投影返回/形参/SSA 值的语义类型。
fn build_function_view(
    program: &CompiledValkyrieProgram,
    canonical: &ValkyrieMirProgram,
    function: &ValkyrieMirFunction,
) -> Result<MachineFunctionView, String> {
    // SSA 值的表示合同：Specialized 或 Reified（且必须有引用实例布局）才可进入目标层。
    for (value, type_id) in &function.value_types {
        let identity = ValueIdentity::new(function.instance, *value);
        match program.representation().value_reps.get(&identity) {
            Some(ValueRepresentation::Specialized) => {}
            Some(ValueRepresentation::Reified) if target_plan::has_reference_instance_layout(canonical, *type_id) => {}
            Some(representation) => {
                return Err(format!(
                    "值 `{identity:?}` 的表示 `{representation:?}` 尚无目标私有载体合同，拒绝按语义类型重新选择表示"
                ));
            }
            None => return Err(format!("值 `{identity:?}` 缺少 RepresentationPlan 载体合同")),
        }
    }
    // 入口参数：SSA 值合同与 canonical 冻结声明名。
    for (index, (value, _)) in function.parameters.iter().enumerate() {
        if !function.value_types.contains_key(value) {
            return Err("入口参数缺少 SSA 值合同".to_string());
        }
        if function.parameter_names.get(index).is_none() {
            return Err(format!("函数实例 `{:?}` 入口参数 {index} 缺少 canonical 冻结的声明参数名", function.instance));
        }
    }
    let invoke_lowerings = &program.representation().invoke_lowerings;
    for block in function.blocks.values() {
        for instruction in &block.instructions {
            // sum 操作必须携带链接期 variant 合同，payload 布局不能由 backend 补造。
            for (nominal, variant) in sum_operations(&instruction.operation) {
                let Some(sum) = canonical.linked.sum_types.iter().find(|sum| sum.nominal == nominal) else {
                    return Err(format!("sum `{nominal:?}` 缺少链接期 variant 合同"));
                };
                if !sum.variants.iter().any(|entry| entry.id == variant) {
                    return Err(format!("sum variant `{nominal:?}/{variant:?}` 缺少链接期 variant 合同"));
                }
            }
            // invoke 降低合同：表示计划必须为每条调用冻结消去方式。
            if let ValkyrieMirOperation::Invoke { callee, .. } = &instruction.operation {
                match (callee, invoke_lowerings.get(&instruction.id)) {
                    (ValkyrieMirCallee::Item(_), Some(InvokeLowering::Direct)) => {}
                    (ValkyrieMirCallee::Witness(_), Some(InvokeLowering::TypedWitness)) => {}
                    (ValkyrieMirCallee::Value(callee), Some(InvokeLowering::TypedReference)) => {
                        let Some(callee_type) = function.value_types.get(callee) else {
                            return Err(format!(
                                "调用指令 `{}` 的 callee `Value({callee:?})` 缺少 Semantic MIR 类型事实",
                                instruction.id.index()
                            ));
                        };
                        if !matches!(canonical.linked.types.get(callee_type).map(|row| &row.kind), Some(CanonicalTypeKind::Function { .. })) {
                            return Err(format!(
                                "调用指令 `{}` 的 callee `Value({callee:?})` 类型 `{callee_type:?}` 不是函数类型，\
                                 尚无目标私有调用合同，拒绝发射间接调用",
                                instruction.id.index()
                            ));
                        }
                    }
                    (_, None) => {
                        return Err(format!("调用指令 `{}` 缺少 RepresentationPlan 降低合同", instruction.id.index()));
                    }
                    (_, Some(lowering)) => {
                        return Err(format!(
                            "调用指令 `{}` 的表示 `{lowering:?}` 与 callee `{callee:?}` 尚无目标私有调用合同，拒绝改用普通调用",
                            instruction.id.index()
                        ));
                    }
                }
            }
        }
        // 终结符支持面：Raise 映射为 Trap，其余 effect/dispatch 在机器码 lane 失败闭合。
        if !matches!(
            &block.terminator,
            ValkyrieMirTerminator::Return { .. }
                | ValkyrieMirTerminator::Jump { .. }
                | ValkyrieMirTerminator::Branch { .. }
                | ValkyrieMirTerminator::Unreachable
                | ValkyrieMirTerminator::PerformEffect { effect: ValkyrieMirEffectKind::Raise, .. }
        ) {
            return Err(format!("canonical effect terminator 尚无目标合同: {:?}", block.terminator));
        }
    }
    let symbol = canonical
        .linked
        .callable_names
        .get(&function.instance)
        .ok_or_else(|| format!("callable 实例 `{:?}` 缺少 ABI 名称", function.instance))?
        .to_string();
    let return_type = target_plan::lower_type(canonical, function.return_type)?;
    let mut param_types = Vec::with_capacity(function.parameters.len());
    for (_, type_id) in &function.parameters {
        param_types.push(target_plan::lower_type(canonical, *type_id)?);
    }
    let mut value_types = BTreeMap::new();
    for (value, type_id) in &function.value_types {
        value_types.insert(*value, target_plan::lower_type(canonical, *type_id)?);
    }
    Ok(MachineFunctionView {
        symbol,
        param_types,
        return_type,
        value_types,
        parameter_names: function.parameter_names.clone(),
    })
}

/// 提取一条 MIR 指令携带的 sum 实例/variant 对（链接期 variant 合同校验用）。
fn sum_operations(operation: &ValkyrieMirOperation) -> Vec<(NominalInstanceId, VariantId)> {
    match operation {
        ValkyrieMirOperation::SumNew { nominal, variant, .. } | ValkyrieMirOperation::SumVariantIs { nominal, variant, .. } => {
            vec![(*nominal, *variant)]
        }
        ValkyrieMirOperation::SumPayloadGet { nominal, variant, .. } => vec![(*nominal, *variant)],
        _ => Vec::new(),
    }
}

/// 入口符号：旧 prepare 的 `abi_name_for_instance(entry).parts().last()` 等价。
pub(crate) fn entry_symbol(ctx: &MachineContext, entry: &ItemInstanceId) -> Option<String> {
    ctx.abi_name(entry).and_then(|name| name.parts().last().map(|part| part.as_str().to_string()))
}
