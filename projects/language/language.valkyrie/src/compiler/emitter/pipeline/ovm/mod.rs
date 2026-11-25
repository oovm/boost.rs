//! OVM lane：语言侧直读 MIR 结算 `.odin` 目标私有输入。
//!
//! 边界（God IR 拆除整改）：本模块是 OVM 纵链唯一的计划事实消费点——可达闭包、
//! 入口槽合同、外部签名、sum 物理布局、导出面与 ABI 合同全部从 `LaneContext`
//! 与 `ValkyrieMirFunction` 一次结算；emitter 只保留目标私有编码原语
//! （`BytecodeEmitter` / `encode_odin_function` / operator 绑定 / ABI 投影），
//! 不再解释跨目标共享程序载荷。

use std::collections::{BTreeMap, BTreeSet};

use compiler_emitter::emitter_backend_vm::{
    BytecodeEmitter, LayoutPolicy, OvmProgramInput, freeze_ovm_operator, ovm_operator_target, resolve_physical_layouts, sanitize_symbol,
    select_unique_target,
};
use compiler_target::ExternalImportLink;
use compiler_types::{ItemInstanceId, LayoutId, SemanticType};
use compiler_types::semantic_ids::NominalInstanceId;
use data_binary_odin::{OdinExport, OdinExportKind, OdinFunction, OdinLayout, OdinModuleData, Odin_VERSION};

use crate::compiler::emitter::pipeline::lane::LaneContext;
use crate::compiler::emitter::pipeline::{AdapterStage, TargetPlanError};

use lowerer::{OvmFunctionLowering, OvmModuleFacts};

pub(crate) mod lowerer;

/// 从 lane 共享上下文结算 OVM 目标私有输入（`.odin` 模块 + 库导出 ABI 合同）。
///
/// 失败闭合：闭包内没有任何可执行函数体、缺身份/缺合同/不支持的操作一律按
/// `TargetPlanError::Unsound` 结构化失败，禁止回退猜测。
pub(crate) fn build_ovm_program_input(ctx: &LaneContext) -> Result<OvmProgramInput, TargetPlanError> {
    let mut module = OdinModuleData {
        version: Odin_VERSION,
        name: format!("{}__{}", sanitize_symbol(ctx.linked().module_name.as_str()), sanitize_symbol(ctx.fragment().id.as_str())),
        constants: Vec::new(),
        functions: Vec::new(),
        imports: Vec::new(),
        exports: Vec::new(),
        witness_entries: Vec::new(),
        code_bytes: Vec::new(),
        globals: Vec::new(),
        init_function_indices: Vec::new(),
        layouts: Vec::new(),
    };

    // 函数表顺序与旧公共路径一致：计划函数表按实例身份升序发射。
    let mut operations: Vec<ItemInstanceId> = ctx.reachable().iter().copied().filter(|instance| ctx.view(instance).is_some()).collect();
    operations.sort();
    if operations.is_empty() {
        return Err(unsound(AdapterStage::Function, None, None, "Nyar VM 后端要求 Compiler 持有的可执行函数体，闭包内没有任何带函数体的实例"));
    }

    // 物理布局解析策略与旧路径同源（OVM 策略：引用槽 8/8、文本槽 4/4）。
    let layouts = resolve_physical_layouts(ctx.aggregate_layouts(), LayoutPolicy::OVM)
        .map_err(|reason| unsound(AdapterStage::Representation, None, None, reason))?;

    let mut layout_index_by_id = BTreeMap::<LayoutId, i32>::new();
    // 定长数组的布局身份不保证进入 `AggregateLayoutPlan`（合成布局由前端按需登记），
    // OVM 的 Odin 槽位又没有类型，因此按字段数铸造后端私有布局，绝不以未冻结身份开新布局。
    let mut layout_index_by_field_count = BTreeMap::<i32, i32>::new();
    let sum_layout_index_by_nominal = register_sum_layouts(ctx, &operations, &mut module.layouts)?;
    let external_link_by_instance =
        ctx.imports().iter().map(|(instance, import)| (*instance, import.links.clone())).collect::<BTreeMap<ItemInstanceId, Vec<ExternalImportLink>>>();
    // 外部调用的实参位宽与内部调用同源：来自冻结的导入签名（`BackendImport.parameter_types`）。
    // 消费点缺失签名时必须结构化失败，不得回落到 semantic MIR 的 `Integer64` 默认。
    let external_param_types_by_instance = ctx
        .imports()
        .iter()
        .map(|(instance, import)| (*instance, import.parameter_types.clone()))
        .collect::<BTreeMap<ItemInstanceId, Vec<SemanticType>>>();
    // `[ovm_operator]` 绑定在 OVM 自己的入口按导入链接冻结：助记符仅用于诊断，
    // arity、操作数改写与头码全部来自 OvmOperatorBinding。
    let external_opcode_by_instance = {
        let mut operators = BTreeMap::new();
        for (instance, import) in ctx.imports() {
            let Some((link, mnemonic)) =
                select_unique_target(&import.links, ovm_operator_target).map_err(|reason| unsound(AdapterStage::Link, Some(*instance), None, reason))?
            else {
                continue;
            };
            let symbol = ctx.abi_name(instance).map(|name| name.to_string()).unwrap_or_default();
            let Some(binding) = freeze_ovm_operator(link, *instance, &symbol, &import.parameter_types, &import.return_type)
                .map_err(|reason| unsound(AdapterStage::Link, Some(*instance), Some(symbol.clone()), reason))?
            else {
                continue;
            };
            operators.insert(*instance, (mnemonic.to_string(), binding));
        }
        operators
    };

    let export_names = nyar_export_name_map(ctx);
    let mut function_index_by_instance = BTreeMap::<ItemInstanceId, i32>::new();
    for (index, instance) in operations.iter().enumerate() {
        function_index_by_instance.insert(*instance, index as i32);
    }
    let entry_slots = build_function_entry_slots(ctx, &operations)?;
    let function_entry_arities = entry_slots.iter().map(|(index, (arity, _))| (*index, *arity)).collect::<BTreeMap<i32, usize>>();
    let param_types_by_function_index = entry_slots.into_iter().map(|(index, (_, types))| (index, types)).collect::<BTreeMap<i32, Vec<SemanticType>>>();
    let param_layouts_by_function_index = build_function_param_layouts(ctx, &operations);

    let services = OvmModuleFacts {
        ctx,
        layouts: &layouts,
        function_index_by_instance: &function_index_by_instance,
        function_entry_arities: &function_entry_arities,
        param_types_by_function_index: &param_types_by_function_index,
        param_layouts_by_function_index: &param_layouts_by_function_index,
        external_link_by_instance: &external_link_by_instance,
        external_param_types_by_instance: &external_param_types_by_instance,
        external_opcode_by_instance: &external_opcode_by_instance,
        sum_layout_index_by_nominal: &sum_layout_index_by_nominal,
    };

    for instance in &operations {
        let export_name = export_names.get(instance).cloned().unwrap_or_else(|| nyar_mir_export_name(ctx, *instance));
        let code_offset = module.code_bytes.len() as i32;
        let constants_base = module.constants.len() as i32;
        let mut emitter = BytecodeEmitter::new(constants_base, &mut module.imports);

        let lowering = OvmFunctionLowering::new(&services, *instance, &mut emitter, &mut module.layouts, &mut layout_index_by_id, &mut layout_index_by_field_count)?;
        let arity = lowering.entry_arity() as i32;
        let local_count = lowering.local_count() as i32;
        // 计划事实 → 目标私有 LIR → `.odin` 函数体编码器；编码器不读 MIR/计划。
        let (function_code, new_constants) = lowering.lower()?;

        module.constants.extend(new_constants);
        module.code_bytes.extend_from_slice(&function_code);

        // 与 JVM/CLR 对齐：调用约定 arity 以入口块 SSA 形参为准；`param_types` 可能含已废弃的 ABI 槽。
        let module_function_index = module.functions.len() as i32;
        module.functions.push(OdinFunction {
            name: export_name.clone(),
            arity,
            local_count: local_count.max(arity),
            code_offset,
            code_length: module.code_bytes.len() as i32 - code_offset,
        });
        if nyar_should_export_operation(ctx, *instance) {
            module.exports.push(OdinExport { kind: OdinExportKind::Function, symbol_name: export_name, function_index: module_function_index });
        }
    }

    // OVM 库导出 ABI 合同：只由冻结的签名 / 布局 / sum 事实派生；任何不支持
    // 的类型都在这里结构化失败，绝不降级成“先跑再说”。合同面与 wasm 的
    // `library_invoke` 同源：只覆盖 `[export]` 公共导出面；`exported_operations`
    // 还包含被链接进来的泛型 std 实例，它们的签名尚未实例化、本来就没有可冻结
    // 的结构，把它们拉进 ABI 合同只会让整个模块无法产出，而不是让调用更安全。
    let abi_exports = ctx
        .fragment()
        .export_names
        .keys()
        .filter_map(|instance| export_names.get(instance).cloned().map(|logical_name| (*instance, logical_name)))
        .collect::<Vec<_>>();
    let abi_contract = if abi_exports.is_empty() {
        None
    }
    else {
        let facts = lowerer::abi_export_facts(ctx, &abi_exports)?;
        let plan_facts = compiler_emitter::ovm_abi::OvmAbiPlanFacts {
            aggregate_layouts: ctx.aggregate_layouts(),
            aggregate_layout_by_semantic_type: ctx.aggregate_layout_by_semantic_type(),
            sum_layout_by_semantic_type: ctx.sum_layout_by_semantic_type(),
            program_text_representation: ctx.program_text_representation(),
        };
        Some(
            compiler_emitter::ovm_abi::derive_contract(&plan_facts, &facts, &mut module.layouts, &mut layout_index_by_id, &sum_layout_index_by_nominal)
                .map_err(|reason| unsound(AdapterStage::Function, None, None, reason))?,
        )
    };

    Ok(OvmProgramInput::new(module, abi_contract))
}

/// 库导出面的逻辑名清单：与模块内 `OdinExport.symbol_name` 同一映射。
///
/// 集合来自共享派生点 [`export_surface`]：`[export]` 冻结名 ∪ 程序入口。ABI 合同、
/// run-contract 入口决议与交付字段都消费这份事实，避免两处各自拼名字。
pub(crate) fn library_public_export_names(ctx: &LaneContext) -> Vec<String> {
    let names = nyar_export_name_map(ctx);
    export_surface(ctx).into_iter().filter_map(|operation| names.get(&operation).cloned()).collect()
}

/// 导出面的实例集合：声明导出名 ∪ 程序入口（顺序与 `export_names` 一致）。
///
/// 可达闭包不是导出面；它只描述模块内部 `Call` 可见性。禁止按「可达即导出」
/// 自行重算——否则带函数类型形参的内部辅助函数会被要求过库边界。
fn export_surface(ctx: &LaneContext) -> Vec<ItemInstanceId> {
    let mut surface = ctx.fragment().export_names.keys().copied().collect::<Vec<_>>();
    if let Some(entry) = ctx.fragment().entry_operation
        && !surface.contains(&entry)
    {
        surface.push(entry);
    }
    surface
}

/// 单个实例是否属于导出面。导出表入表判定都走这一个谓词。
fn nyar_should_export_operation(ctx: &LaneContext, operation: ItemInstanceId) -> bool {
    ctx.fragment().export_names.contains_key(&operation) || ctx.fragment().entry_operation == Some(operation)
}

/// 实例的导出名回落：`[export]` 冻结名优先，否则 ABI 派生名。
fn nyar_mir_export_name(ctx: &LaneContext, operation: ItemInstanceId) -> String {
    if let Some(public_name) = ctx.fragment().export_names.get(&operation) {
        return public_name.clone();
    }
    ctx.abi_name(&operation).map(|name| name.to_string()).unwrap_or_else(|| format!("{operation:?}"))
}

/// 导出面的实例 → 名称映射：公共导出名保持原样；abi 派生名在同一导出面绑定
/// 多个实例时按实例下标消歧，导出表与入口决议共用同一映射。
fn nyar_export_name_map(ctx: &LaneContext) -> BTreeMap<ItemInstanceId, String> {
    let mut bases: BTreeMap<ItemInstanceId, String> = BTreeMap::new();
    for operation in export_surface(ctx) {
        bases.entry(operation).or_insert_with(|| nyar_mir_export_name(ctx, operation));
    }
    let mut groups: BTreeMap<String, Vec<ItemInstanceId>> = BTreeMap::new();
    for (operation, name) in &bases {
        groups.entry(name.clone()).or_default().push(*operation);
    }
    let mut resolved = BTreeMap::new();
    for (name, operations) in groups {
        if operations.len() == 1 {
            resolved.insert(operations[0], name);
            continue;
        }
        for operation in operations {
            // 公共导出名是 run-contract 的稳定表面，冲突组内只给 abi 派生名追加实例下标。
            if ctx.fragment().export_names.contains_key(&operation) {
                resolved.insert(operation, name.clone());
            }
            else {
                resolved.insert(operation, format!("{name}#{}", operation.index()));
            }
        }
    }
    resolved
}

/// 为模块内出现过的 nominal sum 铸造后端私有物理布局，返回 nominal → 布局下标。
///
/// 物理合同由本 lane 固定：槽 0 = tag（i32），槽 1 = payload。字段顺序与 tag 宽度
/// 都是 OVM 后端私有事实，只由计划冻结的 `SumRepresentation` 的 variant 身份驱动；
/// 禁止按 variant 名 / 短名推断槽位。
fn register_sum_layouts(ctx: &LaneContext, operations: &[ItemInstanceId], layouts: &mut Vec<OdinLayout>) -> Result<BTreeMap<NominalInstanceId, i32>, TargetPlanError> {
    use crate::compiler::valkyrie_mir::ValkyrieMirOperation;
    let sums = ctx.sum_representations();
    let mut nominals = BTreeSet::new();
    for instance in operations {
        let Some(function) = ctx.function(instance)
        else {
            continue;
        };
        for block in function.blocks.values() {
            for instruction in &block.instructions {
                let nominal = match &instruction.operation {
                    ValkyrieMirOperation::SumNew { nominal, .. }
                    | ValkyrieMirOperation::SumPayloadGet { nominal, .. }
                    | ValkyrieMirOperation::SumVariantIs { nominal, .. } => *nominal,
                    _ => continue,
                };
                nominals.insert(nominal);
            }
        }
    }
    let mut map = BTreeMap::new();
    for nominal in nominals {
        sums.get(&nominal).ok_or_else(|| unsound(AdapterStage::Representation, None, None, format!("OVM sum 表示缺失：nominal {nominal:?} 没有冻结的 SumTypeLayout")))?;
        let index = layouts.len() as i32;
        layouts.push(OdinLayout { field_count: 2 });
        map.insert(nominal, index);
    }
    Ok(map)
}

/// 在降低函数体之前结算入口槽：arity 与形参类型表。
///
/// 入口块是内部函数调用 arity 与形参槽类型的唯一来源，两份事实一次结算：
/// arity 不再用 `param_types.len()` 顶替，形参表也不再把同一情形当成空参数。
/// 有函数体却缺入口块属目标输入合同破坏，结构化失败，不做回退猜测。
///
/// 常量实参的位宽由形参槽类型决定；缺这张表时，Call 操作数位只能回落到
/// semantic MIR 合同（`Constant::Int` = Integer64），会把 i32 形参静默拓宽。
fn build_function_entry_slots(ctx: &LaneContext, operations: &[ItemInstanceId]) -> Result<BTreeMap<i32, (usize, Vec<SemanticType>)>, TargetPlanError> {
    let mut map = BTreeMap::new();
    for (index, instance) in operations.iter().enumerate() {
        let view = ctx.view(instance).expect("operations 只含已结算函数视图的实例");
        let function = ctx.function(instance).expect("operations 只含带函数体的实例");
        let block = function.blocks.get(&function.entry).ok_or_else(|| {
            unsound(
                AdapterStage::Function,
                Some(*instance),
                Some(view.symbol.clone()),
                format!("OVM 入口槽合同破坏：symbol=`{}` 声明了入口块 {}，但块表中不存在该入口块", view.symbol, function.entry.0),
            )
        })?;
        let slots = lowerer::SlotPlan::plan(function, view).map_err(|reason| unsound(AdapterStage::Function, Some(*instance), Some(view.symbol.clone()), reason))?;
        let types = block
            .parameters
            .iter()
            .map(|(value, _)| {
                slots
                    .value_locals
                    .get(value)
                    .and_then(|local| slots.local_types.get(usize::from(*local)))
                    .cloned()
                    .ok_or_else(|| {
                        unsound(
                            AdapterStage::Function,
                            Some(*instance),
                            Some(view.symbol.clone()),
                            format!("OVM 形参 SSA 值缺少已规划槽类型：%{}（symbol=`{}`）", value.index(), view.symbol),
                        )
                    })
            })
            .collect::<Result<Vec<_>, TargetPlanError>>()?;
        map.insert(index as i32, (types.len(), types));
    }
    Ok(map)
}

/// 被调函数入口形参槽的冻结布局身份表。
///
/// 与 OVM ABI 合同共用同一派生点（[`lowerer::frozen_entry_parameter_layouts`]）：
/// 合同与 lowering 的实际槽布局必须同源，禁止在这里另写一份回落。
fn build_function_param_layouts(ctx: &LaneContext, operations: &[ItemInstanceId]) -> BTreeMap<i32, Vec<Option<LayoutId>>> {
    let mut map = BTreeMap::new();
    for (index, instance) in operations.iter().enumerate() {
        let view = ctx.view(instance).expect("operations 只含已结算函数视图的实例");
        let function = ctx.function(instance).expect("operations 只含带函数体的实例");
        let slots = lowerer::SlotPlan::plan(function, view).expect("入口槽结算已先行校验同一张表");
        map.insert(index as i32, lowerer::frozen_entry_parameter_layouts(ctx, function, view, &slots));
    }
    map
}

/// 结构化失败构造器（与 lane.rs 同一 `TargetPlanError::Unsound` 词汇）。
fn unsound(stage: AdapterStage, instance: Option<ItemInstanceId>, symbol: Option<String>, reason: impl Into<String>) -> TargetPlanError {
    TargetPlanError::Unsound { stage, instance, symbol, reason: reason.into() }
}
