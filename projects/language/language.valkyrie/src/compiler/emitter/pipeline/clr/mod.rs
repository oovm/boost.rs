//! CLR lane 目标私有输入结算：从 `LaneContext` 与 MIR 直出 `ClrProgramInput`。
//!
//! 边界（God IR 拆除整改）：本模块是 CLR 纵链唯一的计划事实消费点——可达闭包、
//! 方法命名与签名、布局/sum 载体类型、入口投影桩与 managed ABI 契约全部从
//! `LaneContext` 与 `ValkyrieMirFunction` 一次结算；emitter 只保留目标私有编码
//! 原语（类型映射、载体定义、operator 绑定、argv 编码表与模块装配自检），
//! 不再解释跨目标共享程序载荷。
//!
//! 失败闭合合同（与旧公共路径逐条对齐）：
//! - 类型映射缺项（缺冻结布局/载体身份）结构化拒绝，禁止名称回退；
//! - witness 分派必须在实例化固定点消去，到达目标层即失败；
//! - 闭包捕获环境在 CLR 没有载体，点名 lambda 实例与捕获名失败；
//! - suspend 策略是分区级事实：片段声明 suspend 能力由装配层
//!   （`assemble_fragment`）失败闭合，本 lane 的 MSIL 编码不为其发明回退形状；
//! - 值聚合复制深度、sum tag/payload、managed ABI argv 子集全部保留。

use std::collections::{BTreeMap, BTreeSet};

use compiler_emitter::emitter_backend_clr::{
    ClrOperatorBinding, ClrProgramInput, GLOBAL_METHODS_TYPE, MANAGED_ENTRY_METHOD, ManagedDispatchTarget, MsilAssembly, MsilMethodBody,
    MsilMethodRef, MsilMethodSignature, MsilModule, MsilOpcode, MsilType, build_layout_types, build_managed_entry_method, build_sum_types,
    call_instruction, clr_operator_target, freeze_clr_operator, ldarg0, managed_abi_type_name, managed_argv_expressible, msil_parameter_type,
    msil_return_type, opcode_instruction, reject_duplicate_type_names, sanitize_operation_symbol, sanitize_symbol, select_unique_target,
};
use compiler_emitter::{ManagedAbiContract, ManagedAbiEntry};
use compiler_target::ClrSuspendStrategy;
use compiler_types::ItemInstanceId;

use crate::compiler::emitter::pipeline::clr::lowerer::{ClrMethodPlan, lower_function};
use crate::compiler::emitter::pipeline::lane::{FunctionView, LaneContext};
use crate::compiler::emitter::pipeline::target_plan::{AdapterStage, TargetPlanError};
use crate::compiler::valkyrie_mir::ValkyrieMirFunction;

pub(crate) mod lowerer;

/// 从 lane 共享上下文结算 CLR 目标私有输入（MSIL 模块 + managed ABI 契约）。
///
/// `clr_suspend_strategy` 是分区级合同事实：suspend 能力门在装配层失败闭合，
/// 本 lane 的 MSIL 编码当前对策略没有分支行为，策略只随请求携带、不被发明使用。
pub(crate) fn build_clr_program_input(ctx: &LaneContext, clr_suspend_strategy: ClrSuspendStrategy) -> Result<ClrProgramInput, TargetPlanError> {
    let _ = clr_suspend_strategy;
    let operations = ctx.reachable();
    if operations.is_empty() {
        return Err(unsound(
            AdapterStage::Function,
            None,
            None,
            "CLR 最小 lowering 没有可导出的可执行函数：片段既无 `export_names` 也无入口操作，拒绝凭空生成模块".to_string(),
        ));
    }

    let mut used_names = BTreeMap::<String, ()>::new();
    let mut method_plans = BTreeMap::<ItemInstanceId, ClrMethodPlan>::new();
    for (ordinal, instance) in operations.iter().enumerate() {
        let view = ctx
            .view(instance)
            .ok_or_else(|| unsound(AdapterStage::Function, Some(*instance), None, format!("CLR 最小 lowering：可达操作 `{instance:?}` 缺少函数视图")))?;
        let function = ctx
            .function(instance)
            .ok_or_else(|| unsound(AdapterStage::Function, Some(*instance), Some(view.symbol.clone()), format!("CLR 最小 lowering：可达操作 `{instance:?}` 缺少函数体")))?;
        let base = ctx
            .fragment()
            .export_names
            .get(instance)
            .cloned()
            .or_else(|| ctx.abi_name(instance).map(|name| sanitize_operation_symbol(name)))
            .unwrap_or_else(|| format!("function_{ordinal}"));
        let base = sanitize_symbol(&base);
        let mut name = base.clone();
        let mut suffix = 0usize;
        while used_names.contains_key(&name) {
            suffix += 1;
            name = format!("{base}__{suffix}");
        }
        used_names.insert(name.clone(), ());
        let signature = function_signature(ctx, function, view)
            .map_err(|reason| unsound(AdapterStage::Function, Some(*instance), Some(view.symbol.clone()), reason))?;
        method_plans.insert(*instance, ClrMethodPlan { name, signature });
    }

    let operator_by_instance = external_operator_by_instance(ctx).map_err(|reason| unsound(AdapterStage::Link, None, None, reason))?;
    let entry_operation = ctx.fragment().entry_operation;
    let layout_types = build_layout_types(ctx.aggregate_layouts()).map_err(|reason| unsound(AdapterStage::Representation, None, None, reason))?;
    let sum_types = build_sum_types(&collect_sum_nominals(ctx), ctx.sum_representations())
        .map_err(|reason| unsound(AdapterStage::Representation, None, None, reason))?;
    let mut module = MsilModule {
        assembly: MsilAssembly { name: sanitize_symbol(ctx.linked().module_name.as_str()), externs: Vec::new() },
        types: Vec::new(),
        global_methods: Vec::new(),
    };
    module.types.extend(layout_types);
    module.types.extend(sum_types);
    reject_duplicate_type_names(&module.types).map_err(|reason| unsound(AdapterStage::Representation, None, None, reason))?;
    for instance in operations {
        let Some(view) = ctx.view(instance)
        else {
            continue;
        };
        let Some(function) = ctx.function(instance)
        else {
            continue;
        };
        let method = method_plans
            .get(instance)
            .cloned()
            .ok_or_else(|| unsound(AdapterStage::Function, Some(*instance), Some(view.symbol.clone()), format!("CLR 最小 lowering：可达操作 `{instance:?}` 缺少方法计划")))?;
        // 有导出面时 PE 入口由合成的 `managed_entry` 独占；单个导出方法不能再标入口。
        // 语言入口返回 `i64` 时 CLR `Main` 不能直接承载，改由下面的入口投影桩标入口。
        let direct_entry = entry_operation == Some(*instance)
            && ctx.fragment().export_names.is_empty()
            && matches!(method.signature.return_type, MsilType::Void | MsilType::Int32 { signed: true });
        module
            .global_methods
            .push(lower_function(ctx, function, view, &method, &method_plans, &operator_by_instance, direct_entry).map_err(|reason| {
                unsound(AdapterStage::Function, Some(*instance), Some(view.symbol.clone()), reason)
            })?);
    }
    if ctx.fragment().export_names.is_empty() {
        if let Some(method) = entry_operation.and_then(|entry| method_plans.get(&entry)) {
            if !matches!(method.signature.return_type, MsilType::Void | MsilType::Int32 { signed: true }) {
                let stub = build_entry_projection_stub(method, &mut used_names)
                    .map_err(|reason| unsound(AdapterStage::Function, entry_operation, None, reason))?;
                module.global_methods.push(stub);
            }
        }
    }

    let managed_abi =
        build_managed_abi_surface(&mut module, ctx, &method_plans).map_err(|reason| unsound(AdapterStage::Representation, None, None, reason))?;
    Ok(ClrProgramInput::new(module, managed_abi))
}

fn unsound(stage: AdapterStage, instance: Option<ItemInstanceId>, symbol: Option<String>, reason: impl Into<String>) -> TargetPlanError {
    TargetPlanError::Unsound { stage, instance, symbol, reason: reason.into() }
}

/// 只为 MIR 中实际出现 sum 指令的 nominal 结算载体类型清单。
fn collect_sum_nominals(ctx: &LaneContext) -> BTreeSet<compiler_types::NominalInstanceId> {
    let mut nominals = BTreeSet::new();
    for instance in ctx.reachable() {
        let Some(function) = ctx.function(instance)
        else {
            continue;
        };
        for block in function.blocks.values() {
            for instruction in &block.instructions {
                match &instruction.operation {
                    crate::compiler::valkyrie_mir::ValkyrieMirOperation::SumNew { nominal, .. }
                    | crate::compiler::valkyrie_mir::ValkyrieMirOperation::SumPayloadGet { nominal, .. }
                    | crate::compiler::valkyrie_mir::ValkyrieMirOperation::SumVariantIs { nominal, .. } => {
                        nominals.insert(*nominal);
                    }
                    _ => {}
                }
            }
        }
    }
    nominals
}

/// 冻结的 `[clr_operator("...")]` 绑定：助记符只用于诊断，配方来自计划。
fn external_operator_by_instance(ctx: &LaneContext) -> Result<BTreeMap<ItemInstanceId, (String, ClrOperatorBinding)>, String> {
    let mut operators = BTreeMap::new();
    for (instance, import) in ctx.imports() {
        let Some((link, mnemonic)) = select_unique_target(&import.links, clr_operator_target)?
        else {
            continue;
        };
        let symbol = ctx.abi_name(instance).map(|name| name.to_string()).unwrap_or_default();
        let Some(binding) = freeze_clr_operator(link, *instance, &symbol, &import.parameter_types, &import.return_type)?
        else {
            continue;
        };
        operators.insert(*instance, (mnemonic.to_string(), binding));
    }
    Ok(operators)
}

/// 入口块形参 → 冻结方法签名（与旧公共路径 `function_signature` 同一合同）。
fn function_signature(ctx: &LaneContext, function: &ValkyrieMirFunction, view: &FunctionView) -> Result<MsilMethodSignature, String> {
    let layouts = ctx.aggregate_layouts();
    let entry = function
        .blocks
        .get(&function.entry)
        .ok_or_else(|| format!("CLR 最小 lowering：函数 `{}` 缺少入口块", view.symbol))?;
    let mut parameter_types = Vec::with_capacity(entry.parameters.len());
    for (parameter, _) in &entry.parameters {
        let ty = view
            .value_types
            .get(parameter)
            .ok_or_else(|| format!("CLR 最小 lowering：函数 `{}` 入口形参缺少冻结类型", view.symbol))?;
        parameter_types.push(msil_parameter_type(layouts, ty)?);
    }
    Ok(MsilMethodSignature::new(msil_return_type(layouts, &view.return_type)?, parameter_types))
}

/// 为 CLR 入口投影语言入口的返回值。
///
/// CLR `Main` 只能返回 `void` / `int32`，而语言入口可以返回 `i64`（进程退出码）。
/// 这里合成一个只做调用的入口桩：`i64` 在宿主边界按 Windows 退出码宽度截断成
/// `int32`。其它非 `void`/`int32` 返回没有宿主投影合同，失败闭合，不得用
/// `ldc.i4.0` 冒充成功。
fn build_entry_projection_stub(entry: &ClrMethodPlan, used_names: &mut BTreeMap<String, ()>) -> Result<MsilMethodBody, String> {
    if !matches!(entry.signature.return_type, MsilType::Int64 { signed: true }) {
        return Err(format!(
            "CLR 入口返回类型 `{}` 没有宿主投影合同：只支持 `void`/`int32` 直接入口与 `i64` 退出码截断投影",
            entry.signature.return_type
        ));
    }
    let parameters_ok = match entry.signature.parameter_types.as_slice() {
        [] => true,
        [MsilType::SzArray(element)] => matches!(element.as_ref(), MsilType::String),
        _ => false,
    };
    if !parameters_ok {
        return Err(format!(
            "CLR 入口形参 `{}` 不符合 CLR entry 合同（只接受无参或 `string[]`）",
            entry.signature.parameter_list_text()
        ));
    }
    let stub_name = unique_method_name(format!("{}__entry", entry.name), used_names);
    let mut instructions = Vec::new();
    if !entry.signature.parameter_types.is_empty() {
        instructions.push(ldarg0());
    }
    instructions.push(call_instruction(MsilMethodRef { owner: None, name: entry.name.clone(), signature: entry.signature.clone() }));
    instructions.push(opcode_instruction(MsilOpcode::ConvI4));
    instructions.push(opcode_instruction(MsilOpcode::Ret));
    Ok(MsilMethodBody {
        method: MsilMethodRef {
            owner: None,
            name: stub_name,
            signature: MsilMethodSignature::new(MsilType::Int32 { signed: true }, entry.signature.parameter_types.clone()),
        },
        locals: Vec::new(),
        instructions,
        max_stack: 1,
        is_entry_point: true,
        is_async: false,
    })
}

/// 在已有名字集合里取一个不冲突的方法名，并登记占用。
fn unique_method_name(base: String, used_names: &mut BTreeMap<String, ()>) -> String {
    let mut name = base.clone();
    let mut suffix = 0usize;
    while used_names.contains_key(&name) {
        suffix += 1;
        name = format!("{base}__{suffix}");
    }
    used_names.insert(name.clone(), ());
    name
}

/// 为导出入口生成 managed ABI 契约，并仅为 argv 可表达的入口合成分发入口
/// `managed_entry`。
///
/// 每个导出入口都无条件记录为 ABI 事实：`parameter_types` / `return_type` 来自
/// 托管 ABI 词汇表，数组、引用、unsigned 都是合法库签名。argv launcher 是宿主
/// 命令行能力，只覆盖可无损解析的标量子集；不可表达时 `launcher_class` 为
/// `None`，既不合成分发分支，也不因此让整个模块失败。
fn build_managed_abi_surface(
    module: &mut MsilModule,
    ctx: &LaneContext,
    method_plans: &BTreeMap<ItemInstanceId, ClrMethodPlan>,
) -> Result<Option<ManagedAbiContract>, String> {
    let export_names = &ctx.fragment().export_names;
    if export_names.is_empty() {
        return Ok(None);
    }

    let mut entries = Vec::with_capacity(export_names.len());
    let mut dispatch_targets = Vec::new();
    for (instance, source_name) in export_names {
        // 声明身份按「有导出注解用注解名，否则源码名」合同取；源码声明名是
        // 独立冻结事实，两个轴不得在契约里互相复制。
        let logical_entry = ctx.fragment().export_annotations.get(instance).cloned().unwrap_or_else(|| source_name.clone());
        let method = method_plans
            .get(instance)
            .ok_or_else(|| format!("CLR managed ABI：导出入口 `{logical_entry}` 的实例 `{instance:?}` 没有已降低方法"))?;
        let body = module
            .global_methods
            .iter()
            .find(|candidate| candidate.method.name == method.name && candidate.method.signature == method.signature)
            .ok_or_else(|| format!("CLR managed ABI：导出入口 `{logical_entry}` 的方法 `{}` 不在模块中", method.name))?;
        let function = ctx
            .function(instance)
            .ok_or_else(|| format!("CLR managed ABI：导出入口 `{logical_entry}` 的实例 `{instance:?}` 缺少函数体"))?;
        // 形参声明名的唯一权威是冻结的入口参数名；数量/空名/重名非法时
        // 结构化失败，禁止按位置或类型名猜测，也不写空名字伪造。
        let parameter_names = frozen_entry_parameter_names(&logical_entry, function)?;
        let parameter_types = method.signature.parameter_types.iter().map(managed_abi_type_name).collect::<Result<Vec<_>, String>>()?;
        let return_type = managed_abi_type_name(&method.signature.return_type)?;
        // 「事实」与「能力」分离：入口签名无条件记录，argv launcher 只覆盖可无损编组子集。
        let argv_expressible = parameter_types.iter().all(|ty| managed_argv_expressible(ty)) && managed_argv_expressible(&return_type);
        let launcher_class = argv_expressible.then(|| MANAGED_ENTRY_METHOD.to_string());
        if argv_expressible {
            // 目标下标取自 `entries` 的当前长度，过滤只影响是否 push，不会与入口错位。
            dispatch_targets.push(ManagedDispatchTarget {
                entry_index: entries.len(),
                signature: body.method.signature.clone(),
                parameter_types: parameter_types.clone(),
                return_type: return_type.clone(),
            });
        }
        entries.push(ManagedAbiEntry {
            logical_entry,
            source_name: source_name.clone(),
            owner: GLOBAL_METHODS_TYPE.to_string(),
            method: method.name.clone(),
            signature: method.signature.to_string(),
            parameter_types,
            parameter_names,
            return_type,
            launcher_class,
        });
    }

    if !dispatch_targets.is_empty() {
        // 只在真正合成 `managed_entry` 时检查双入口/重名：没有 argv 目标就不需要该入口。
        if let Some(existing) = module.global_methods.iter().find(|method| method.is_entry_point) {
            return Err(format!("CLR managed ABI：模块已有可执行入口 `{}`，拒绝再合成 `managed_entry` 造成双入口", existing.method.name));
        }
        if module.global_methods.iter().any(|method| method.method.name == MANAGED_ENTRY_METHOD) {
            return Err(format!("CLR managed ABI：已有全局方法 `{MANAGED_ENTRY_METHOD}`，拒绝复用该名字"));
        }
        let entry_method = build_managed_entry_method(&entries, &dispatch_targets)?;
        module.global_methods.push(entry_method);
    }
    // 产物文件名由驱动层最终确定；这里先留空，桥接层负责补全并刷新摘要。
    Ok(Some(ManagedAbiContract::new("clr", String::new(), entries)))
}

/// 入口形参声明名的冻结合同（与旧公共路径 `canonical_parameter_identities` 同源）：
/// 数量与入口块形参一致、非空、不重复；非法时结构化失败，禁止猜测。
fn frozen_entry_parameter_names(logical_entry: &str, function: &ValkyrieMirFunction) -> Result<Vec<String>, String> {
    let entry = function
        .blocks
        .get(&function.entry)
        .ok_or_else(|| format!("CLR managed ABI：导出入口 `{logical_entry}` 缺少入口块"))?;
    let expected = entry.parameters.len();
    if function.parameter_names.len() != expected {
        return Err(format!(
            "CLR managed ABI：导出入口 `{logical_entry}` 的冻结参数名数量 {} 与入口块形参数量 {expected} 不符",
            function.parameter_names.len()
        ));
    }
    let mut names = Vec::with_capacity(expected);
    for name in &function.parameter_names {
        if name.is_empty() {
            return Err(format!("CLR managed ABI：导出入口 `{logical_entry}` 的冻结参数名存在空名，拒绝伪造位置名"));
        }
        if names.iter().any(|existing: &String| existing == name) {
            return Err(format!("CLR managed ABI：导出入口 `{logical_entry}` 的冻结参数名 `{name}` 重复，拒绝猜测"));
        }
        names.push(name.clone());
    }
    Ok(names)
}
