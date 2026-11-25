//! JVM lane：语言侧直读 MIR 结算目标私有 `class` 输入。
//!
//! 边界（God IR 拆除整改）：本模块是 JVM 纵链唯一的计划事实消费点——可达闭包、
//! 方法命名与 descriptor、参数名冻结校验、布局/sum companion 类、managed ABI
//! 契约与 argv launcher、入口 launcher 身份全部从 `LaneContext` 与
//! `ValkyrieMirFunction` 一次结算；emitter 只保留目标私有编码原语
//! （descriptor 派生、槽位种类、operator 绑定、argv 编码表与类装配自检），
//! 不再解释跨目标共享程序载荷。

use std::collections::{BTreeMap, BTreeSet};

use compiler_emitter::emitter_backend_jvm::{
    ArgvKind, JvmClassFile, JvmCodeBody, JvmFieldRef, JvmInstruction, JvmMethodDescriptor, JvmMethodRef, JvmMethodSignature, JvmOperatorBinding,
    JvmProgramInput, JvmTypeDescriptor, argv_encoding, freeze_jvm_operator, jvm_operator_target, jvm_parameter_descriptor, jvm_return_descriptor,
    sanitize_jvm_method_symbol, sanitize_symbol, select_unique_target,
};
use compiler_emitter::{ManagedAbiContract, ManagedAbiEntry};
use compiler_types::{ItemInstanceId, SemanticType};

use crate::compiler::emitter::pipeline::lane::{FunctionView, LaneContext};
use crate::compiler::emitter::pipeline::{AdapterStage, TargetPlanError};
use crate::compiler::valkyrie_mir::ValkyrieMirFunction;

use lowerer::{JvmMethodPlan, JVM_ACC_PUBLIC, JVM_ACC_STATIC, build_layout_classes, build_sum_classes, lower_function};

pub(crate) mod lowerer;

/// 从 lane 共享上下文结算 JVM 目标私有输入（主类方法体 + companion 载体 + ABI 契约）。
///
/// 失败闭合：闭包内没有任何可执行函数体、缺身份/缺合同/不支持的操作一律按
/// `TargetPlanError::Unsound` 结构化失败，禁止回退猜测。
pub(crate) fn build_jvm_program_input(ctx: &LaneContext) -> Result<JvmProgramInput, TargetPlanError> {
    let operations = ctx.reachable();
    if operations.is_empty() {
        return Err(unsound(
            AdapterStage::Function,
            None,
            None,
            "JVM 最小 lowering 没有可导出的可执行函数：片段既无 `export_names` 也无入口操作，拒绝凭空生成 class",
        ));
    }

    let mut used_names = BTreeMap::<String, ()>::new();
    let mut method_plans = BTreeMap::<ItemInstanceId, JvmMethodPlan>::new();
    let mut logical_signatures = BTreeMap::<ItemInstanceId, (Vec<SemanticType>, SemanticType)>::new();
    let mut logical_parameter_names = BTreeMap::<ItemInstanceId, Vec<String>>::new();
    for (ordinal, instance) in operations.iter().enumerate() {
        let view = ctx.view(instance).ok_or_else(|| unsound(AdapterStage::Function, Some(*instance), None, "JVM 最小 lowering：可达操作缺少函数视图"))?;
        let function = ctx.function(instance).ok_or_else(|| unsound(AdapterStage::Function, Some(*instance), None, "JVM 最小 lowering：可达操作缺少函数体"))?;
        logical_signatures.insert(*instance, (view.param_types.clone(), view.return_type.clone()));
        // 导出入口的形参声明名必须从冻结的 `function.parameter_names` 结算；身份
        // 不合法（数量/空名/重名）直接失败，不写空名字伪造。
        if ctx.fragment().export_names.contains_key(instance) {
            let names = frozen_entry_parameter_names(&view.symbol, function).map_err(|reason| unsound(AdapterStage::Function, Some(*instance), Some(view.symbol.clone()), reason))?;
            logical_parameter_names.insert(*instance, names);
        }
        // 公开名映射按冻结合同从片段取：有导出注解用注解名，否则源码名。
        let base = ctx
            .fragment()
            .export_annotations
            .get(instance)
            .or(ctx.fragment().export_names.get(instance))
            .cloned()
            .or_else(|| ctx.abi_name(instance).map(|name| sanitize_jvm_method_symbol(name)))
            .unwrap_or_else(|| format!("function_{ordinal}"));
        let base = sanitize_symbol(&base);
        let mut name = base.clone();
        let mut suffix = 0usize;
        while used_names.contains_key(&name) {
            suffix += 1;
            name = format!("{base}__{suffix}");
        }
        used_names.insert(name.clone(), ());
        let descriptor = function_descriptor(ctx, function, view).map_err(|reason| unsound(AdapterStage::Function, Some(*instance), Some(view.symbol.clone()), reason))?;
        method_plans.insert(*instance, JvmMethodPlan { name, descriptor });
    }

    let internal_name = format!(
        "nyar/{}_{}",
        sanitize_symbol(ctx.linked().module_name.as_str()),
        sanitize_symbol(ctx.fragment().id.as_str())
    );
    let operator_by_instance = external_operator_by_instance(ctx).map_err(|reason| unsound(AdapterStage::Link, None, None, reason))?;
    let mut methods = Vec::with_capacity(operations.len());
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
            .ok_or_else(|| unsound(AdapterStage::Function, Some(*instance), Some(view.symbol.clone()), "JVM 最小 lowering：可达操作缺少方法计划"))?;
        methods.push(lower_function(ctx, &internal_name, function, view, &method, &method_plans, &operator_by_instance).map_err(|reason| unsound(AdapterStage::Function, Some(*instance), Some(view.symbol.clone()), reason))?);
    }

    // 程序入口只由计划身份决定；入口名不参与「是否有 launcher」的判定。
    // 入口同时是导出入口时复用 ABI 入口已构建的 launcher，不重复构建。
    let entry_binding = ctx.fragment().entry_operation.and_then(|entry| {
        ctx.abi_name(&entry).map(|name| {
            let logical = name.parts().last().map(|part| part.as_str().to_string()).unwrap_or_else(|| "main".to_string());
            (entry, logical)
        })
    });
    // 公开名映射按冻结合同从片段取：有导出注解用注解名，否则源码名。
    let public_names = ctx
        .fragment()
        .export_names
        .keys()
        .filter_map(|instance| {
            ctx.fragment()
                .export_annotations
                .get(instance)
                .or(ctx.fragment().export_names.get(instance))
                .cloned()
                .map(|name| (*instance, name))
        })
        .collect::<BTreeMap<_, _>>();
    let (launcher_classes, managed_abi, entry_launcher) = build_managed_abi_surface(
        &internal_name,
        &methods,
        ctx.fragment().export_names.clone(),
        &public_names,
        &method_plans,
        &logical_signatures,
        &logical_parameter_names,
        entry_binding,
    )
    .map_err(|reason| unsound(AdapterStage::Representation, None, None, reason))?;
    let mut companion_classes = build_layout_classes(ctx.aggregate_layouts()).map_err(|reason| unsound(AdapterStage::Representation, None, None, reason))?;
    companion_classes.extend(build_sum_classes(ctx).map_err(|reason| unsound(AdapterStage::Representation, None, None, reason))?);
    companion_classes.extend(launcher_classes);
    Ok(JvmProgramInput::new(internal_name, methods, companion_classes, managed_abi, entry_launcher))
}

fn unsound(stage: AdapterStage, instance: Option<ItemInstanceId>, symbol: Option<String>, reason: impl Into<String>) -> TargetPlanError {
    TargetPlanError::Unsound { stage, instance, symbol, reason: reason.into() }
}

/// 导出入口的形参声明名冻结校验（`canonical_parameter_identities` 同规则）：
/// 数量与声明参数一致、非空、互不重复；违规结构化失败，不伪造名字。
fn frozen_entry_parameter_names(symbol: &str, function: &ValkyrieMirFunction) -> Result<Vec<String>, String> {
    if function.parameter_names.len() != function.parameters.len() {
        return Err(format!(
            "JVM 导出入口 `{symbol}` 的形参声明名数量与参数数量不一致: names={} parameters={}",
            function.parameter_names.len(),
            function.parameters.len()
        ));
    }
    let mut used = BTreeSet::new();
    for (index, name) in function.parameter_names.iter().enumerate() {
        if name.trim().is_empty() {
            return Err(format!("JVM 导出入口 `{symbol}` 的形参 {index} 声明名为空"));
        }
        if !used.insert(name.clone()) {
            return Err(format!("JVM 导出入口 `{symbol}` 的形参声明名 `{name}` 重复"));
        }
    }
    Ok(function.parameter_names.clone())
}

/// 冻结的 `[jvm_operator("...")]` 绑定：助记符只用于诊断，配方来自计划。
fn external_operator_by_instance(ctx: &LaneContext) -> Result<BTreeMap<ItemInstanceId, (String, JvmOperatorBinding)>, String> {
    let mut operators = BTreeMap::new();
    for (instance, import) in ctx.imports() {
        let Some((link, mnemonic)) = select_unique_target(&import.links, jvm_operator_target)?
        else {
            continue;
        };
        let symbol = ctx.abi_name(instance).map(|name| name.to_string()).unwrap_or_default();
        let Some(binding) = freeze_jvm_operator(link, *instance, &symbol, &import.parameter_types, &import.return_type)
            .map_err(|reason| format!("JVM operator 绑定 `{symbol}` 冻结失败: {reason}"))?
        else {
            continue;
        };
        operators.insert(*instance, (mnemonic.to_string(), binding));
    }
    Ok(operators)
}

fn function_descriptor(ctx: &LaneContext, function: &ValkyrieMirFunction, view: &FunctionView) -> Result<JvmMethodDescriptor, String> {
    let layouts = ctx.aggregate_layouts();
    let entry = function.blocks.get(&function.entry).ok_or_else(|| format!("JVM 最小 lowering：函数 `{}` 缺少入口块", view.symbol))?;
    let mut parameter_types = Vec::with_capacity(entry.parameters.len());
    for (parameter, _) in &entry.parameters {
        let ty = view.value_types.get(parameter).cloned().ok_or_else(|| format!("JVM 最小 lowering：函数 `{}` 入口形参缺少冻结类型", view.symbol))?;
        parameter_types.push(jvm_parameter_descriptor(layouts, &ty)?);
    }
    Ok(JvmMethodDescriptor::new(parameter_types, jvm_return_descriptor(layouts, &view.return_type)?))
}

/// 汇总 managed ABI 契约，并只为 argv 可表达的入口生成 launcher。
///
/// 入口必须已经以 `public static` 方法落在主类中；缺失方法返回结构化错误。
/// 入口事实（逻辑类型名、描述符）与 argv 包装能力分开：数组、引用等签名仍是
/// 合法的库入口，只是 `launcher_class = None`，不生成 companion 类。
fn build_managed_abi_surface(
    internal_name: &str,
    methods: &[JvmMethodSignature],
    export_names: BTreeMap<ItemInstanceId, String>,
    public_names: &BTreeMap<ItemInstanceId, String>,
    method_plans: &BTreeMap<ItemInstanceId, JvmMethodPlan>,
    logical_signatures: &BTreeMap<ItemInstanceId, (Vec<SemanticType>, SemanticType)>,
    logical_parameter_names: &BTreeMap<ItemInstanceId, Vec<String>>,
    entry: Option<(ItemInstanceId, String)>,
) -> Result<(Vec<JvmClassFile>, Option<ManagedAbiContract>, Option<String>), String> {
    let mut entries = Vec::with_capacity(export_names.len());
    let mut companion_classes = Vec::with_capacity(export_names.len());
    let mut used_launcher_names = BTreeSet::new();
    let mut entry_launcher = None;
    for (instance, source_name) in &export_names {
        // 声明身份按「有导出注解用注解名，否则源码名」合同取；源码声明名是
        // 独立冻结事实，两个轴不得在契约里互相复制。
        let logical_entry = public_names.get(instance).cloned().unwrap_or_else(|| source_name.clone());
        let method = method_plans
            .get(instance)
            .ok_or_else(|| format!("JVM managed ABI：导出入口 `{logical_entry}` 的实例 `{instance:?}` 没有已降低方法"))?;
        let lowered = methods
            .iter()
            .find(|candidate| candidate.name == method.name && candidate.descriptor == method.descriptor)
            .ok_or_else(|| format!("JVM managed ABI：导出入口 `{logical_entry}` 的方法 `{}` 不在主类中", method.name))?;
        if lowered.access_flags & (JVM_ACC_PUBLIC | JVM_ACC_STATIC) != (JVM_ACC_PUBLIC | JVM_ACC_STATIC) {
            return Err(format!(
                "JVM managed ABI：导出入口 `{logical_entry}` 的方法 `{}` 不是 `public static`，拒绝生成 launcher",
                method.name
            ));
        }
        // 逻辑 ABI 类型来自冻结的 `SemanticType` 签名，物理 descriptor 另存；不得从
        // 已擦除 signedness 的 descriptor 反推作者声明的逻辑类型。
        let (logical_params, logical_return) = logical_signatures
            .get(instance)
            .ok_or_else(|| format!("JVM managed ABI：导出入口 `{logical_entry}` 的实例 `{instance:?}` 缺少冻结逻辑签名"))?;
        let parameter_types = logical_params.iter().map(managed_abi_logical_type_name).collect::<Result<Vec<_>, _>>()?;
        let return_type = managed_abi_logical_type_name(logical_return)?;
        // 形参声明名来自冻结签名，按声明顺序与 `parameter_types` 等长；
        // 缺失即结构化失败，不用空名字或位置猜测填补。
        let parameter_names = logical_parameter_names
            .get(instance)
            .ok_or_else(|| format!("JVM managed ABI：导出入口 `{logical_entry}` 的实例 `{instance:?}` 缺少冻结形参声明名"))?
            .clone();
        // argv launcher 只是包装能力：只有全部形参与返回都能由宿主命令行无损
        // 解析时才生成；否则入口照常写进契约，但 `launcher_class` 为 `None`。
        let argv_expressible = logical_params.iter().chain(std::iter::once(logical_return)).all(|ty| argv_expressible_type_name(ty).is_ok());
        let launcher_class = if argv_expressible {
            let launcher_class = managed_launcher_internal_name(&logical_entry, &mut used_launcher_names)?;
            if entry.as_ref().is_some_and(|(entry_instance, _)| entry_instance == instance) {
                // 程序入口同时是导出入口：复用该 ABI 入口的 launcher 身份。
                entry_launcher = Some(launcher_class.clone());
            }
            companion_classes.push(build_managed_launcher_class(
                &launcher_class,
                internal_name,
                method,
                &parameter_types,
                &return_type,
            )?);
            Some(launcher_class)
        }
        else {
            None
        };
        entries.push(ManagedAbiEntry {
            logical_entry,
            source_name: source_name.clone(),
            owner: internal_name.to_string(),
            method: method.name.clone(),
            signature: method.descriptor.to_string(),
            parameter_types,
            parameter_names,
            return_type,
            launcher_class,
        });
    }
    // 程序入口 launcher：入口身份冻结在计划里，不依赖方法名扫描。
    //
    // argv launcher 只在入口签名可被 `main(String[])` 表达时生成；引用形参等
    // 无法从 argv 编组的入口仍按普通函数降低，但不伪造 launcher（包装能力
    // 属于平台，不能替入口发明形参）。入口同时是导出入口时已在 ABI 循环中
    // 复用 launcher，不再重复构建。
    if let Some((instance, logical_entry)) = entry {
        if entry_launcher.is_none() {
            let method =
                method_plans.get(&instance).ok_or_else(|| format!("JVM 程序入口 `{logical_entry}` 的实例 `{instance:?}` 没有已降低方法"))?;
            let lowered = methods
                .iter()
                .find(|candidate| candidate.name == method.name && candidate.descriptor == method.descriptor)
                .ok_or_else(|| format!("JVM 程序入口 `{logical_entry}` 的方法 `{}` 不在主类中", method.name))?;
            let signature = logical_signatures.get(&instance);
            let argv_expressible = signature.is_some_and(|(params, return_type)| {
                params.iter().chain(std::iter::once(return_type)).all(|ty| argv_expressible_type_name(ty).is_ok())
            });
            if argv_expressible {
                if lowered.access_flags & (JVM_ACC_PUBLIC | JVM_ACC_STATIC) != (JVM_ACC_PUBLIC | JVM_ACC_STATIC) {
                    return Err(format!("JVM 程序入口 `{logical_entry}` 的方法 `{}` 不是 `public static`，拒绝生成 launcher", method.name));
                }
                let (params, return_type) = signature.ok_or_else(|| {
                    format!("JVM 程序入口 `{logical_entry}` 的实例 `{instance:?}` 缺少冻结逻辑签名")
                })?;
                let parameter_types = params.iter().map(managed_abi_logical_type_name).collect::<Result<Vec<_>, _>>()?;
                let return_type = managed_abi_logical_type_name(return_type)?;
                let launcher_class = managed_launcher_internal_name(&logical_entry, &mut used_launcher_names)?;
                companion_classes.push(build_managed_launcher_class(
                    &launcher_class,
                    internal_name,
                    method,
                    &parameter_types,
                    &return_type,
                )?);
                entry_launcher = Some(launcher_class);
            }
        }
    }
    let managed_abi = if entries.is_empty() {
        None
    }
    else {
        // 产物文件名由驱动层最终确定；这里先留空，桥接层负责补全并刷新摘要。
        Some(ManagedAbiContract::new("jvm", String::new(), entries))
    };
    Ok((companion_classes, managed_abi, entry_launcher))
}

/// managed ABI 的逻辑类型名：入口事实，只来自冻结的 `SemanticType` 签名。
///
/// 范围严格对齐本 lane 已支持的物理载体（`jvm_type_descriptor` /
/// `jvm_array_element_descriptor` / `function_descriptor`）：标量按声明保留
/// signedness 与宽度，数组递归到元素类型后追加 `[]`。没有物理载体合同的形状
/// 一律结构化失败，不猜名字、不回退到擦除后的 descriptor。
fn managed_abi_logical_type_name(ty: &SemanticType) -> Result<String, String> {
    Ok(match ty {
        SemanticType::Bottom | SemanticType::Unit => "void".to_string(),
        SemanticType::Boolean => "bool".to_string(),
        SemanticType::Character => "char".to_string(),
        SemanticType::Integer8 { signed: true } => "i8".to_string(),
        SemanticType::Integer8 { signed: false } => "u8".to_string(),
        SemanticType::Integer16 { signed: true } => "i16".to_string(),
        SemanticType::Integer16 { signed: false } => "u16".to_string(),
        SemanticType::Integer32 { signed: true } => "i32".to_string(),
        SemanticType::Integer32 { signed: false } => "u32".to_string(),
        SemanticType::Integer64 { signed: true } => "i64".to_string(),
        SemanticType::Integer64 { signed: false } => "u64".to_string(),
        SemanticType::Float32 => "f32".to_string(),
        SemanticType::Float64 => "f64".to_string(),
        SemanticType::Utf8 | SemanticType::Utf16 => "string".to_string(),
        // 堆数组有元素载体合同；固定长度数组没有 ABI 长度事实，走下面的结构化拒绝。
        SemanticType::Array(element) => format!("{}[]", managed_abi_logical_type_name(element)?),
        SemanticType::Integer128 { .. } => {
            return Err(format!("JVM managed ABI 不支持 `{ty}` 作为入口类型：缺少 128 位标量载体合同"));
        }
        SemanticType::Tuple(..) => {
            return Err(format!("JVM managed ABI 不支持 `{ty}` 作为入口类型：裸 tuple 没有冻结布局身份，缺少 ABI 载体合同"));
        }
        SemanticType::Function(..) => {
            return Err(format!("JVM managed ABI 不支持 `{ty}` 作为入口类型：函数值没有 ABI 载体合同"));
        }
        SemanticType::Nominal { .. } | SemanticType::Apply(..) => {
            return Err(format!("JVM managed ABI 不支持 `{ty}` 作为入口类型：名义/泛型实例缺少 ABI 布局身份合同"));
        }
        SemanticType::FixedArray { .. } => {
            return Err(format!("JVM managed ABI 不支持 `{ty}` 作为入口类型：固定长度数组缺少 ABI 长度事实"));
        }
        SemanticType::Nullable(..) => {
            return Err(format!("JVM managed ABI 不支持 `{ty}` 作为入口类型：可空标量需要 sum 表示，缺少 ABI 载体合同"));
        }
        other => {
            return Err(format!("JVM managed ABI 不支持 `{other}` 作为入口类型：缺少该形状的 ABI 载体合同"));
        }
    })
}

/// argv 可表达性：`SemanticType` → 载体逻辑名 → 共享编码表。
///
/// 「入口事实」（[`managed_abi_logical_type_name`]）与「包装能力」
/// （[`argv_encoding`]）分离：这里只做投影，是否可表达以及拒绝理由全部来自
/// 唯一合同表，不在此另写一份类型名单。
fn argv_expressible_type_name(ty: &SemanticType) -> Result<String, String> {
    if let SemanticType::Integer128 { .. } = ty {
        return Err(format!("argv launcher 不支持 `{ty}`：128 位整型宽于宿主标量（最宽 64 位），没有对应解析器"));
    }
    let logical = managed_abi_logical_type_name(ty)?;
    argv_encoding(&logical)?;
    Ok(logical)
}

/// launcher 内部名：`nyar/invoke/{sanitize(logical_entry)}`，非字母数字转 `_`。
fn managed_launcher_internal_name(logical_entry: &str, used: &mut BTreeSet<String>) -> Result<String, String> {
    let component = logical_entry.chars().map(|character| if character.is_ascii_alphanumeric() { character } else { '_' }).collect::<String>();
    if component.is_empty() {
        return Err(format!("JVM managed ABI：导出入口 `{logical_entry}` 无法生成合法 launcher 类名"));
    }
    let internal_name = format!("nyar/invoke/{component}");
    if !used.insert(internal_name.clone()) {
        return Err(format!("JVM managed ABI：多个导出入口映射到同一 launcher 类 `{internal_name}`"));
    }
    Ok(internal_name)
}

/// 构造 `public static void main(String[])` argv launcher。
///
/// 栈顺序：先压 `System.out`，再按共享合同表逐参数解析 argv，调用主类静态方法，
/// 最后按合同表输出返回值；`void` 入口只调用不输出。逻辑载体名与物理描述符必须
/// 由同一合同的载体投影一致，否则结构化失败（不允许用 `f64` 顶替 `f32` 之类的漂移）。
fn build_managed_launcher_class(
    launcher_internal_name: &str,
    owner: &str,
    method: &JvmMethodPlan,
    logical_parameter_types: &[String],
    logical_return_type: &str,
) -> Result<JvmClassFile, String> {
    if logical_parameter_types.len() != method.descriptor.parameter_types.len() {
        return Err(format!(
            "JVM argv launcher：入口 `{launcher_internal_name}` 的逻辑形参数（{}）与物理描述符形参数（{}）不一致",
            logical_parameter_types.len(),
            method.descriptor.parameter_types.len()
        ));
    }
    let return_carrier = argv_carrier_descriptor(logical_return_type)?;
    if return_carrier != method.descriptor.return_type {
        return Err(format!(
            "JVM argv launcher：入口 `{launcher_internal_name}` 的返回载体 `{logical_return_type}` 合同投影 {return_carrier} 与方法描述符 {} 不一致",
            method.descriptor.return_type
        ));
    }
    let mut launcher = JvmClassFile::new(launcher_internal_name.to_string());
    let mut instructions = Vec::new();
    let print_result = !matches!(argv_encoding(logical_return_type)?.kind, ArgvKind::Void);
    if print_result {
        instructions.push(JvmInstruction::GetStatic(JvmFieldRef {
            owner: "java/lang/System".to_string(),
            name: "out".to_string(),
            descriptor: JvmTypeDescriptor::Object("java/io/PrintStream".to_string()),
        }));
    }
    let mut argument_slots = 0u16;
    for (index, parameter) in method.descriptor.parameter_types.iter().enumerate() {
        let index = i32::try_from(index).map_err(|_| format!("JVM managed ABI launcher 形参序号超出 `int` 载体: {index}"))?;
        let logical_parameter = &logical_parameter_types[index as usize];
        let carrier = argv_carrier_descriptor(logical_parameter)?;
        if carrier != *parameter {
            return Err(format!(
                "JVM argv launcher：入口 `{launcher_internal_name}` 的形参 `{logical_parameter}` 合同投影 {carrier} 与物理描述符 {parameter} 不一致"
            ));
        }
        instructions.push(JvmInstruction::ALoad0);
        instructions.push(JvmInstruction::IConst(index));
        instructions.push(JvmInstruction::AALoad);
        instructions.extend(argv_parse_instructions(logical_parameter, index)?);
        argument_slots = argument_slots.saturating_add(parameter.slot_count());
    }
    instructions.push(JvmInstruction::InvokeStatic(JvmMethodRef {
        owner: owner.to_string(),
        name: method.name.clone(),
        descriptor: method.descriptor.clone(),
    }));
    if print_result {
        instructions.extend(argv_result_print_instructions(logical_return_type)?);
    }
    instructions.push(JvmInstruction::Return);

    // 越界/非法字面量路径要构造异常对象（`new` + `dup` + `ldc`），固定额外 3 槽。
    let max_stack = u16::from(print_result).saturating_add(argument_slots).saturating_add(3);
    launcher.methods.push(JvmMethodSignature {
        name: "main".to_string(),
        descriptor: JvmMethodDescriptor::new(
            vec![JvmTypeDescriptor::array(JvmTypeDescriptor::Object("java/lang/String".to_string()))],
            JvmTypeDescriptor::Void,
        ),
        access_flags: JVM_ACC_PUBLIC | JVM_ACC_STATIC,
        code: Some(JvmCodeBody { max_stack, max_locals: 1, instructions }),
    });
    Ok(launcher)
}

/// 共享合同表 → JVM 物理载体描述符。
///
/// launcher 的栈形状必须与目标方法描述符同构：无符号/窄整型落到宿主 `int`，
/// `u64` 落到 `long`，`f32` 必须是真实 `Float`（`F`），不得升格成 `f64`。
fn argv_carrier_descriptor(logical_type: &str) -> Result<JvmTypeDescriptor, String> {
    Ok(match argv_encoding(logical_type)?.kind {
        ArgvKind::Void => JvmTypeDescriptor::Void,
        ArgvKind::Bool => JvmTypeDescriptor::Boolean,
        ArgvKind::Signed { bits: 32 } | ArgvKind::Unsigned { bits: 8 | 16 | 32 } => JvmTypeDescriptor::Int,
        ArgvKind::Signed { bits: 64 } | ArgvKind::Unsigned { bits: 64 } => JvmTypeDescriptor::Long,
        ArgvKind::Float { bits: 32 } => JvmTypeDescriptor::Float,
        ArgvKind::Float { bits: 64 } => JvmTypeDescriptor::Double,
        other => {
            return Err(format!("JVM argv launcher：载体 `{logical_type}` 的合同类别 {other:?} 没有 JVM 物理描述符"));
        }
    })
}

/// 按共享合同表生成「弹出 argv 字符串 → 载体值」的指令序列。
///
/// 调用前栈顶已是 `Strings[argument_index]`；序列结束后栈顶是目标形参的 JVM
/// 载体值，与 [`argv_carrier_descriptor`] 一致。
fn argv_parse_instructions(logical_type: &str, argument_index: i32) -> Result<Vec<JvmInstruction>, String> {
    let encoding = argv_encoding(logical_type)?;
    let mut instructions = Vec::new();
    match encoding.kind {
        ArgvKind::Void => return Err(format!("JVM argv launcher 形参不能是 `void`（载体 `{logical_type}`）")),
        ArgvKind::Bool => instructions.extend(strict_boolean_parse(argument_index)),
        ArgvKind::Signed { bits: 32 } => {
            instructions.push(JvmInstruction::InvokeStatic(parse_method_ref("java/lang/Integer", "parseInt", JvmTypeDescriptor::Int)));
        }
        ArgvKind::Signed { bits: 64 } => {
            instructions.push(JvmInstruction::InvokeStatic(parse_method_ref("java/lang/Long", "parseLong", JvmTypeDescriptor::Long)));
        }
        // 宿主没有精确的 unsigned 8/16 位解析器：用更宽的有符号解析器 + 合同表给出的
        // 显式范围检查；越界抛出未捕获 `NumberFormatException`（宿主非零退出）。
        ArgvKind::Unsigned { bits: 8 | 16 } => {
            instructions.push(JvmInstruction::InvokeStatic(parse_method_ref("java/lang/Integer", "parseInt", JvmTypeDescriptor::Int)));
            let max = encoding
                .unsigned_max
                .ok_or_else(|| format!("JVM argv launcher：合同表缺少 `{logical_type}` 的无符号上界"))?;
            instructions.extend(unsigned_range_check(logical_type, argument_index, max)?);
        }
        // 宿主无符号解析器宽度与语义宽度一致，越界由宿主直接抛 `NumberFormatException`。
        ArgvKind::Unsigned { bits: 32 } => {
            instructions.push(JvmInstruction::InvokeStatic(parse_method_ref("java/lang/Integer", "parseUnsignedInt", JvmTypeDescriptor::Int)));
        }
        ArgvKind::Unsigned { bits: 64 } => {
            instructions.push(JvmInstruction::InvokeStatic(parse_method_ref("java/lang/Long", "parseUnsignedLong", JvmTypeDescriptor::Long)));
        }
        ArgvKind::Float { bits: 32 } => {
            instructions.push(JvmInstruction::InvokeStatic(parse_method_ref("java/lang/Float", "parseFloat", JvmTypeDescriptor::Float)));
        }
        ArgvKind::Float { bits: 64 } => {
            instructions.push(JvmInstruction::InvokeStatic(parse_method_ref("java/lang/Double", "parseDouble", JvmTypeDescriptor::Double)));
        }
        other => return Err(format!("JVM argv launcher 不支持形参载体 `{logical_type}`（合同类别 {other:?}）")),
    }
    Ok(instructions)
}

/// 构造接受单个 `String` 的宿主静态解析调用。
fn parse_method_ref(owner: &str, name: &str, return_type: JvmTypeDescriptor) -> JvmMethodRef {
    JvmMethodRef {
        owner: owner.to_string(),
        name: name.to_string(),
        descriptor: JvmMethodDescriptor::new(
            vec![JvmTypeDescriptor::Object("java/lang/String".to_string())],
            return_type,
        ),
    }
}

/// `PrintStream.println` 重载。
fn print_method_ref(parameter: JvmTypeDescriptor) -> JvmMethodRef {
    JvmMethodRef {
        owner: "java/io/PrintStream".to_string(),
        name: "println".to_string(),
        descriptor: JvmMethodDescriptor::new(vec![parameter], JvmTypeDescriptor::Void),
    }
}

/// 精确宿主调用：`owner.name(parameter) -> return_type`。
fn exact_method_ref(owner: &str, name: &str, parameter: JvmTypeDescriptor, return_type: JvmTypeDescriptor) -> JvmMethodRef {
    JvmMethodRef { owner: owner.to_string(), name: name.to_string(), descriptor: JvmMethodDescriptor::new(vec![parameter], return_type) }
}

/// 严格 `bool`：先 `trim()` 掉首尾空白，再只接受 ASCII 大小写不敏感的
/// `true` / `false`，其余抛 `NumberFormatException`；不使用会把任意串当 `false`
/// 的 `Boolean.parseBoolean`。
fn strict_boolean_parse(argument_index: i32) -> Vec<JvmInstruction> {
    let true_label = format!("argv_{argument_index}_bool_true");
    let bad_label = format!("argv_{argument_index}_bool_bad");
    let done_label = format!("argv_{argument_index}_bool_done");
    let equals = exact_method_ref(
        "java/lang/String",
        "equalsIgnoreCase",
        JvmTypeDescriptor::Object("java/lang/String".to_string()),
        JvmTypeDescriptor::Boolean,
    );
    let mut instructions = vec![
        JvmInstruction::InvokeVirtual(JvmMethodRef {
            owner: "java/lang/String".to_string(),
            name: "trim".to_string(),
            descriptor: JvmMethodDescriptor::new(
                Vec::new(),
                JvmTypeDescriptor::Object("java/lang/String".to_string()),
            ),
        }),
        JvmInstruction::Dup,
        JvmInstruction::LdcString("true".to_string()),
        JvmInstruction::InvokeVirtual(equals.clone()),
        JvmInstruction::IfNe(true_label.clone()),
        JvmInstruction::Dup,
        JvmInstruction::LdcString("false".to_string()),
        JvmInstruction::InvokeVirtual(equals),
        JvmInstruction::IfEq(bad_label.clone()),
        JvmInstruction::Pop,
        JvmInstruction::IConst(0),
        JvmInstruction::Goto(done_label.clone()),
        JvmInstruction::Label(true_label),
        JvmInstruction::Pop,
        JvmInstruction::IConst(1),
        JvmInstruction::Goto(done_label.clone()),
        JvmInstruction::Label(bad_label),
    ];
    instructions.extend(throw_instructions(
        "java/lang/NumberFormatException",
        format!("argv 参数 {argument_index} 不是 true/false（bool 只接受 true 或 false）"),
    ));
    instructions.push(JvmInstruction::Label(done_label));
    instructions
}

/// 构造并抛出 `new {class}("{message}")`；栈上只保留既有值。
fn throw_instructions(class: &str, message: String) -> Vec<JvmInstruction> {
    vec![
        JvmInstruction::New(class.to_string()),
        JvmInstruction::Dup,
        JvmInstruction::LdcString(message),
        JvmInstruction::InvokeSpecial(JvmMethodRef {
            owner: class.to_string(),
            name: "<init>".to_string(),
            descriptor: JvmMethodDescriptor::new(
                vec![JvmTypeDescriptor::Object("java/lang/String".to_string())],
                JvmTypeDescriptor::Void,
            ),
        }),
        JvmInstruction::AThrow,
    ]
}

/// 无符号范围检查：`0 <= value <= max`，越界抛 `NumberFormatException`。
fn unsigned_range_check(logical_type: &str, argument_index: i32, max: u128) -> Result<Vec<JvmInstruction>, String> {
    let max = i32::try_from(max)
        .map_err(|_| format!("JVM argv launcher：`{logical_type}` 的无符号上界 {max} 超出 `int` 载体"))?;
    let bad_label = format!("argv_{argument_index}_range_bad");
    let ok_label = format!("argv_{argument_index}_range_ok");
    let mut instructions = vec![
        JvmInstruction::Dup,
        JvmInstruction::IfLt(bad_label.clone()),
        JvmInstruction::Dup,
        JvmInstruction::IConst(max),
        JvmInstruction::IfICmpGt(bad_label.clone()),
        JvmInstruction::Goto(ok_label.clone()),
        JvmInstruction::Label(bad_label),
    ];
    instructions.extend(throw_instructions(
        "java/lang/NumberFormatException",
        format!("argv 参数 {argument_index} 超出 {logical_type} 范围 0..={max}"),
    ));
    instructions.push(JvmInstruction::Label(ok_label));
    Ok(instructions)
}

/// 返回值输出：按合同类别选择 `println` 重载；无符号返回值先用宿主 `toUnsigned*`
/// 转成能无损打印的载体，避免把 `u8 200` 打成 `-56`。
fn argv_result_print_instructions(logical_type: &str) -> Result<Vec<JvmInstruction>, String> {
    let encoding = argv_encoding(logical_type)?;
    let mut instructions = Vec::new();
    match encoding.kind {
        ArgvKind::Void => return Err("JVM argv launcher：`void` 结果不得生成输出调用".to_string()),
        ArgvKind::Bool => instructions.push(JvmInstruction::InvokeVirtual(print_method_ref(JvmTypeDescriptor::Boolean))),
        ArgvKind::Signed { bits: 32 } => instructions.push(JvmInstruction::InvokeVirtual(print_method_ref(JvmTypeDescriptor::Int))),
        ArgvKind::Signed { bits: 64 } => instructions.push(JvmInstruction::InvokeVirtual(print_method_ref(JvmTypeDescriptor::Long))),
        ArgvKind::Unsigned { bits: 8 } => {
            instructions.push(JvmInstruction::InvokeStatic(exact_method_ref(
                "java/lang/Byte",
                "toUnsignedInt",
                JvmTypeDescriptor::Byte,
                JvmTypeDescriptor::Int,
            )));
            instructions.push(JvmInstruction::InvokeVirtual(print_method_ref(JvmTypeDescriptor::Int)));
        }
        ArgvKind::Unsigned { bits: 16 } => {
            instructions.push(JvmInstruction::InvokeStatic(exact_method_ref(
                "java/lang/Short",
                "toUnsignedInt",
                JvmTypeDescriptor::Short,
                JvmTypeDescriptor::Int,
            )));
            instructions.push(JvmInstruction::InvokeVirtual(print_method_ref(JvmTypeDescriptor::Int)));
        }
        ArgvKind::Unsigned { bits: 32 } => {
            instructions.push(JvmInstruction::InvokeStatic(exact_method_ref(
                "java/lang/Integer",
                "toUnsignedLong",
                JvmTypeDescriptor::Int,
                JvmTypeDescriptor::Long,
            )));
            instructions.push(JvmInstruction::InvokeVirtual(print_method_ref(JvmTypeDescriptor::Long)));
        }
        ArgvKind::Unsigned { bits: 64 } => {
            instructions.push(JvmInstruction::InvokeStatic(exact_method_ref(
                "java/lang/Long",
                "toUnsignedString",
                JvmTypeDescriptor::Long,
                JvmTypeDescriptor::Object("java/lang/String".to_string()),
            )));
            instructions.push(JvmInstruction::InvokeVirtual(print_method_ref(
                JvmTypeDescriptor::Object("java/lang/String".to_string()),
            )));
        }
        ArgvKind::Float { bits: 32 } => instructions.push(JvmInstruction::InvokeVirtual(print_method_ref(JvmTypeDescriptor::Float))),
        ArgvKind::Float { bits: 64 } => instructions.push(JvmInstruction::InvokeVirtual(print_method_ref(JvmTypeDescriptor::Double))),
        other => return Err(format!("JVM argv launcher 不支持返回载体 `{logical_type}`（合同类别 {other:?}）")),
    }
    Ok(instructions)
}

#[cfg(test)]
mod jvm_lane_tests {
    use super::*;
    use compiler_emitter::emitter_backend_jvm::{JvmMethodSignature, jvm_layout_class_name, jvm_type_descriptor};
    use compiler_target::Identifier;
    use compiler_types::{AggregateLayout, AggregateLayoutPlan, FieldLayout, ItemInstanceId, SemanticType, StorageKind};
    use lowerer::jvm_layout_field_name;

    fn closed_apply() -> SemanticType {
        SemanticType::Apply(
            Box::new(SemanticType::Named(Identifier::new("std::collection::ArrayList"))),
            vec![SemanticType::Integer64 { signed: true }],
        )
    }

    fn plan_with_closed_apply(register: bool) -> AggregateLayoutPlan {
        let apply = closed_apply();
        let outer = AggregateLayout {
            id: 0,
            name: "Holder".to_string(),
            namespace: String::new(),
            key: None,
            storage: StorageKind::Reference,
            fields: vec![FieldLayout { name: "items".to_string(), ty: apply.clone() }],
        };
        let inner = AggregateLayout {
            id: 1,
            name: "ArrayList".to_string(),
            namespace: String::new(),
            key: None,
            storage: StorageKind::Reference,
            fields: vec![FieldLayout { name: "len".to_string(), ty: SemanticType::Integer64 { signed: true } }],
        };
        let mut plan = AggregateLayoutPlan { layouts: vec![outer, inner], ..Default::default() };
        if register {
            plan.layout_by_semantic_type.insert(apply, 1);
        }
        plan
    }

    #[test]
    fn layout_classes_register_identity_before_resolving_fields() {
        let plan = plan_with_closed_apply(true);
        let classes = build_layout_classes(&plan).expect("字段引用必须命中已登记载体");
        assert_eq!(classes.len(), 2);
        assert_eq!(classes[0].fields[0].descriptor, compiler_emitter::emitter_backend_jvm::JvmTypeDescriptor::Object(jvm_layout_class_name(&plan.layouts[1])));
        assert_eq!(classes[0].fields[0].name, jvm_layout_field_name(0));
    }

    /// ABI 词汇表是入口事实：保留声明宽度与 signedness，数组递归到元素类型。
    #[test]
    fn abi_vocabulary_covers_declared_scalars_and_arrays() {
        let i64_array = SemanticType::Array(Box::new(SemanticType::Integer64 { signed: true }));
        assert_eq!(managed_abi_logical_type_name(&i64_array), Ok("i64[]".to_string()));
        assert_eq!(managed_abi_logical_type_name(&SemanticType::Utf8), Ok("string".to_string()));
        assert_eq!(managed_abi_logical_type_name(&SemanticType::Integer32 { signed: false }), Ok("u32".to_string()));
        assert_eq!(managed_abi_logical_type_name(&SemanticType::Float32), Ok("f32".to_string()));
    }

    /// argv 是包装能力而非 ABI 事实：数组、字符串、字符与宽标量仍被合同表拒绝。
    #[test]
    fn argv_subset_rejects_unrepresentable_types() {
        let unrepresentable = [
            SemanticType::Array(Box::new(SemanticType::Integer64 { signed: true })),
            SemanticType::Utf8,
            SemanticType::Utf16,
            SemanticType::Character,
            SemanticType::Integer8 { signed: true },
            SemanticType::Integer16 { signed: true },
            SemanticType::Integer128 { signed: true },
        ];
        for ty in unrepresentable {
            assert!(argv_expressible_type_name(&ty).is_err(), "`{ty}` 不应被 argv 表达");
        }
    }

    /// `f32` 与无符号整型由共享合同表列入可表达集：宿主有真实载体与解析器。
    #[test]
    fn argv_subset_admits_f32_and_unsigned_integers() {
        let representable = [
            SemanticType::Float32,
            SemanticType::Integer8 { signed: false },
            SemanticType::Integer16 { signed: false },
            SemanticType::Integer32 { signed: false },
            SemanticType::Integer64 { signed: false },
        ];
        for ty in representable {
            assert!(argv_expressible_type_name(&ty).is_ok(), "`{ty}` 应被 argv 表达");
        }
    }

    /// f32 / u8 / u64 入口必须生成 launcher，且解析调用指向真实宿主方法。
    #[test]
    fn launcher_uses_shared_contract_host_parsers() {
        let descriptor =
            JvmMethodDescriptor::new(vec![JvmTypeDescriptor::Float, JvmTypeDescriptor::Int, JvmTypeDescriptor::Long], JvmTypeDescriptor::Float);
        let method = JvmMethodPlan { name: "score".to_string(), descriptor };
        let logical_parameters = vec!["f32".to_string(), "u8".to_string(), "u64".to_string()];
        let launcher = build_managed_launcher_class("nyar/invoke/score", "nyar/App", &method, &logical_parameters, "f32")
            .expect("f32/u8/u64 入口必须生成 launcher");
        let code = launcher.methods[0].code.as_ref().expect("launcher main 必须有代码");
        let calls = code
            .instructions
            .iter()
            .filter_map(|instruction| match instruction {
                JvmInstruction::InvokeStatic(reference) | JvmInstruction::InvokeVirtual(reference) => Some(reference),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(
            calls
                .iter()
                .any(|reference| reference.owner == "java/lang/Float"
                    && reference.name == "parseFloat"
                    && reference.descriptor.to_string() == "(Ljava/lang/String;)F"),
            "f32 必须走 `Float.parseFloat` 并落到 `F` 载体: {calls:?}"
        );
        assert!(
            calls.iter().any(|reference| reference.owner == "java/lang/Integer"
                && reference.name == "parseInt"
                && reference.descriptor.to_string() == "(Ljava/lang/String;)I"),
            "u8 必须先用更宽的有符号解析器 + 显式范围检查: {calls:?}"
        );
        assert!(
            calls.iter().any(|reference| reference.owner == "java/lang/Long"
                && reference.name == "parseUnsignedLong"
                && reference.descriptor.to_string() == "(Ljava/lang/String;)J"),
            "u64 必须走宿主无符号解析器: {calls:?}"
        );
        assert!(
            calls.iter().any(|reference| reference.owner == "java/io/PrintStream"
                && reference.name == "println"
                && reference.descriptor.to_string() == "(F)V"),
            "f32 返回值必须用 `println(float)`，不得升格为 double: {calls:?}"
        );
        assert!(
            code.instructions.iter().any(|instruction| matches!(instruction, JvmInstruction::AThrow)),
            "u8 越界必须抛出未捕获异常，而不是静默截断"
        );
    }

    /// `bool` 必须 fail-closed：先无参 `trim()`，再严格比较，否则抛异常。
    #[test]
    fn strict_bool_parse_trims_then_rejects_unknown_literals() {
        let instructions = strict_boolean_parse(2);
        let calls = instructions
            .iter()
            .filter_map(|instruction| match instruction {
                JvmInstruction::InvokeVirtual(reference) => Some(reference),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            calls
                .iter()
                .any(|reference| reference.owner == "java/lang/String"
                    && reference.name == "trim"
                    && reference.descriptor.to_string() == "()Ljava/lang/String;"),
            "bool 解析必须先调用无参 `String.trim()`: {calls:?}"
        );
        assert!(
            calls.iter().all(|reference| reference.name != "parseBoolean"),
            "不得使用 fail-open 的 `Boolean.parseBoolean`"
        );
        assert!(
            instructions.iter().any(|instruction| matches!(instruction, JvmInstruction::AThrow)),
            "非 true/false 必须抛出未捕获异常"
        );
    }

    /// u8 的范围检查与 `athrow` 必须真的能编码成字节码。
    #[test]
    fn launcher_with_unsigned_range_check_encodes() {
        let descriptor = JvmMethodDescriptor::new(vec![JvmTypeDescriptor::Int], JvmTypeDescriptor::Void);
        let method = JvmMethodPlan { name: "take".to_string(), descriptor };
        let launcher =
            build_managed_launcher_class("nyar/invoke/take", "nyar/App", &method, &["u8".to_string()], "void").expect("u8 入口必须生成 launcher");
        let bytes = launcher.to_bytes().expect("带范围检查与 athrow 的 launcher 必须可编码");
        assert_eq!(&bytes[0..4], &[0xCA, 0xFE, 0xBA, 0xBE], "必须产出 class 文件头");
        assert!(bytes.contains(&0xBF), "编码后必须包含 athrow 操作码 0xBF");
    }

    /// 数组形参的导出入口仍是合法 ABI 事实，只是没有 argv launcher。
    #[test]
    fn array_parameter_export_keeps_entry_without_launcher() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let descriptor =
            JvmMethodDescriptor::new(vec![JvmTypeDescriptor::array(JvmTypeDescriptor::Long), JvmTypeDescriptor::Long], JvmTypeDescriptor::Long);
        let methods = vec![JvmMethodSignature {
            name: "twoSum".to_string(),
            descriptor: descriptor.clone(),
            access_flags: JVM_ACC_PUBLIC | JVM_ACC_STATIC,
            code: None,
        }];
        let method_plans = BTreeMap::from([(instance, JvmMethodPlan { name: "twoSum".to_string(), descriptor })]);
        let logical_signatures = BTreeMap::from([(
            instance,
            (
                vec![SemanticType::Array(Box::new(SemanticType::Integer64 { signed: true })), SemanticType::Integer64 { signed: true }],
                SemanticType::Integer64 { signed: true },
            ),
        )]);
        let logical_parameter_names = BTreeMap::from([(instance, vec!["nums".to_string(), "target".to_string()])]);
        let export_names = BTreeMap::from([(instance, "twoSum".to_string())]);

        let (companion_classes, managed_abi, entry_launcher) = build_managed_abi_surface(
            "nyar/App",
            &methods,
            export_names.clone(),
            &export_names,
            &method_plans,
            &logical_signatures,
            &logical_parameter_names,
            None,
        )
        .expect("数组导出入口必须作为 ABI 事实产出");

        assert!(companion_classes.is_empty(), "不可 argv 表达的入口不得生成 launcher 类");
        assert_eq!(entry_launcher, None);
        let contract = managed_abi.expect("导出入口必须进入契约");
        assert_eq!(contract.entries.len(), 1);
        assert_eq!(contract.entries[0].parameter_types, vec!["i64[]".to_string(), "i64".to_string()]);
        assert_eq!(contract.entries[0].return_type, "i64");
        assert_eq!(contract.entries[0].launcher_class, None);
    }
}
