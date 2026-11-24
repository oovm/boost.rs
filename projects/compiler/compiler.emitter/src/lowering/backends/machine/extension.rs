//! 正式编译 lane 的宿主扩展产物降低（Node-API / CPython）。
//!
//! 与残差产品复用同一条目标执行链：计划事实 → 文本 LIR（[`super::x86_64`]）→
//! 唯一程序装配器（[`super::x64_program`]）→ 承载服务（[`super::carrier`]）→
//! 共享库容器（[`super::container`]）。本模块只负责把**语言适配器已冻结的导出
//! 事实**（`export_names` 与函数签名）映射成宿主边界合同，不要求任何特定函数名、
//! 不合成 `main`/`value` 属性，也不重新决定产物用途。
//!
//! 宿主边界能力在这里闭合，未闭合的组合在写出前结构化拒绝：
//!
//! - 导出实参闭合完整 `i64`（Node-API 按 `napi_get_value_bigint_int64` 只接受
//!   JS `bigint` 并用 `lossless` 检查 `i64` 域；CPython 按 `PyArg_ParseTuple("n")`
//!   接收完整 `i64`）、`[i64]` 序列投影（Node-API 逐元素编组并写回；CPython 不
//!   提供该载体）与 `utf8` 文本（Node-API 按 UTF-8 字节复制；CPython 按 `O` 收对象
//!   再经 `PyUnicode_AsUTF8AndSize` 取字节与长度，保留内嵌 NUL）。窄整数没有无损
//!   载体，拒绝而不是静默扩展；
//! - 导出返回闭合 `Integer64 → Int64`、`Boolean → Bool`、`Utf8 → 宿主字符串`
//!   与 `[i64] → 宿主序列`（序列复制只收 Node-API；guest 数组归语言值图，桥在
//!   调用返回后复制、不释放）。其余返回载体仍写出前拒绝；
//! - 宿主扩展不接受进程入口：程序有入口与程序公开若干函数是两个独立事实；
//! - Windows CPython 的导入 DLL 名必须由构建环境提供，版本相关名不能由编译器猜。

use std::collections::BTreeMap;

use crate::{
    FragmentSubmission, Result, backend_plan_views::SemanticType, emitter_backend_machine::HostProperty, lowering::sanitize_symbol,
    lowering::shared::physical_layout::{LayoutPolicy, resolve_physical_layouts},
};
use compiler_types::ItemInstanceId;

use super::{
    HostMethod, HostModule, HostParameterKind, HostValueKind, X64Platform, X64ProductTarget, any_returns_bool, any_returns_int64_array,
    any_returns_utf8, append_text_arena,
    compile_shared_container, is_c_identifier, node_addon_registration, node_method_wrapper, node_module_data, python_method_wrapper,
    python_module_data, python_module_exec, python_module_registration,
    x64_program::encode_x64_program_with_dependencies,
    x86_64::{collect_reachable_functions, lower_function_to_text_program},
};

/// 宿主扩展降低结果：完整共享库镜像与宿主装载事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X64LoweredExtension {
    /// 共享库镜像字节（`PE32+` DLL 或 `ELF64 ET_DYN`，由平台身份决定）。
    pub executable: Vec<u8>,
    /// 宿主装载入口符号（`napi_register_module_v1` / `PyInit_<module>`）。
    pub entry_symbol: String,
    /// 宿主可见导出面（源码名 + 本目标命名合同投影后的公开名，按冻结导出面顺序）。
    pub host_exports: Vec<MachineHostExport>,
    /// 产物扩展名（含点）：`.node` / `.pyd` / `.so`。
    pub artifact_extension: String,
    /// CPython 导入模块名；文件名、`PyInit_<module>`、执行清单与宿主 `import`
    /// 共用这一个值。Node 扩展不使用模块名，为 `None`。
    pub module_name: Option<String>,
}

/// CPython 扩展的导入模块名：工程名与模块名是两个独立事实。
///
/// 工程名保留仓库/题库身份（例如 `leetcode-multiply-strings`），模块名必须是
/// 合法 Python/C 标识符（例如 `leetcode_multiply_strings`）。归一化确定性：
/// 非 `[A-Za-z0-9_]` 字符替换为 `_`，首字符为数字时前置 `_`，结果为空则拒绝。
pub fn cpython_module_name(project_name: &str) -> Result<String> {
    let mut name: String =
        project_name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
    if name.is_empty() {
        return Err(format!("CPython 扩展模块名不能为空（工程名 `{project_name}` 归一化后为空）"));
    }
    if name.as_bytes()[0].is_ascii_digit() {
        name.insert(0, '_');
    }
    Ok(name)
}

/// 机器码交付的一条宿主导出：语言侧源码名与目标公开名。
///
/// Node 扩展的命名合同是「有注解用注解名，否则源码名自动 camelCase 投影」；
/// CPython 的命名合同是源码名。公开名是宿主装载面，源码名保留用于跨目标追溯。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineHostExport {
    /// 语言侧冻结的源码声明名。
    pub source_name: String,
    /// 目标命名合同投影后的宿主公开名。
    pub public_name: String,
    /// 形参宿主载体标签（`i64` / `i64[]` / `utf8`，按形参顺序）；由冻结签名导出，
    /// 供执行清单的宿主消费者区分整数、序列与文本，不在消费侧重新猜测。
    pub parameters: Vec<String>,
    /// 返回宿主载体标签（`i64` / `bool` / `utf8` / `i64[]` / `unit`）。
    pub returns: String,
}

/// 把已冻结的目标计划降低为宿主扩展共享库。
///
/// `target` 必须来自统一目标解析服务的宿主合同；`module_name` 是工程名
/// （Node-API 不使用模块名，但仍参与产物身份命名；CPython 由 [`cpython_module_name`]
/// 归一化为合法导入名，工程名本身保持不变）。
pub fn lower_fragment_to_machine_extension(
    submission: &FragmentSubmission,
    target: &X64ProductTarget,
    module_name: &str,
) -> Result<X64LoweredExtension> {
    let (platform, host) = match target {
        X64ProductTarget::Extension { platform, module, .. } => (*platform, *module),
        X64ProductTarget::Library(_) => return Err("目标不是宿主扩展合同；普通共享库交付尚未接线".to_string()),
    };
    let abi = platform.abi();
    // CPython 的模块名先行归一化：文件名、`PyInit_`、清单与 `import` 必须同值。
    let python_module = match host {
        HostModule::Python => Some(cpython_module_name(module_name)?),
        HostModule::Node => None,
    };

    // 宿主扩展不接受进程入口：入口是应用交付事实，与导出面互相独立。
    if let Some(entry) = submission.backend_plan.entry_operation() {
        return Err(format!("宿主扩展不接受进程入口（`{entry:?}`）；导出面与入口是两个独立事实，应用交付请改用 native 可执行目标"));
    }

    let reachable = collect_reachable_functions(submission);
    if reachable.is_empty() {
        return Err(format!("宿主扩展 `{}` 没有可达函数：冻结导出面为空，拒绝产出无导出的扩展", target.name()));
    }

    // 冻结导出面 → 类型化宿主边界事实。
    let methods = frozen_host_methods(submission, target, &reachable)?;
    if methods.is_empty() {
        return Err(format!("宿主扩展 `{}` 的导出面为空：宿主扩展必须至少公开一个已冻结的导出函数", target.name()));
    }

    // 符号名：导出函数取冻结导出名的合法符号，内部闭包函数取实例身份名。
    let names = extension_symbol_names(submission, target, &reachable, &methods)?;
    // 与可执行 lane 使用同一物理布局策略：宿主扩展的对象字段偏移不得另算一套。
    let layouts = resolve_physical_layouts(submission.backend_plan.aggregate_layouts(), LayoutPolicy::X64)
        .map_err(|reason| format!("宿主扩展降低：物理布局解析失败：{reason}"))?;
    let mut functions = Vec::with_capacity(reachable.len() + methods.len() + 2);
    for instance in &reachable {
        let view = submission.backend_plan.get_function(instance).ok_or_else(|| format!("宿主扩展降低：可达操作 `{instance:?}` 缺少函数体"))?;
        functions.push(lower_function_to_text_program(
            instance,
            submission,
            &view.function,
            &reachable,
            &names,
            platform,
            &layouts,
        )?);
    }

    let host_library = host_import_library(target, platform)?;
    // `ELF` 没有 IAT：宿主库以 `DT_NEEDED` 显式声明（名字来自冻结的宿主 ABI 配置）。
    let host_dependencies: Vec<String> = match target.cpython_import_library() {
        Some(library) if !platform.is_windows() && matches!(target.host_module(), Some(HostModule::Python)) => vec![library.to_string()],
        _ => Vec::new(),
    };
    let properties: &[HostProperty] = &[];

    let (image, entry_symbol, artifact_extension) = match host {
        HostModule::Node => {
            let (node_data, property_name_offsets, method_name_offsets, error_message_offset, range_error_offset, allocation_error_offset) =
                node_module_data(properties, &methods);
            let mut data = node_data;
            let arena_offset = Some(append_text_arena(&mut data));
            let bool_exports = any_returns_bool(properties, &methods);
            for method in &methods {
                functions.push(node_method_wrapper(
                    method,
                    error_message_offset,
                    range_error_offset,
                    allocation_error_offset,
                    platform,
                    &host_library,
                    arena_offset,
                ));
            }
            functions.push(node_addon_registration(
                properties,
                &property_name_offsets,
                methods.as_slice(),
                &method_name_offsets,
                platform,
                &host_library,
                arena_offset,
            ));
            let imports = node_imports(&methods, bool_exports, &host_library);
            let image = encode_x64_program_with_dependencies(&functions, &imports, abi, &data, &[], &host_dependencies)?;
            (image, "napi_register_module_v1".to_string(), ".node".to_string())
        }
        HostModule::Python => {
            let module = python_module.as_deref().expect("Python 分支已冻结模块名");
            if !is_c_identifier(module) {
                return Err(format!("CPython 扩展模块名 `{module}` 不是合法 C 标识符：`PyInit_<module>` 必须能在 C 中声明"));
            }
            let (definition, relocations, property_name_offsets, format_offsets) = python_module_data(module, properties, &methods);
            let mut data = definition;
            let arena_offset = Some(append_text_arena(&mut data));
            let bool_exports = any_returns_bool(properties, &methods);
            for (method, format_offset) in methods.iter().zip(format_offsets) {
                functions.push(python_method_wrapper(method, format_offset, &host_library, arena_offset, platform));
            }
            functions.push(python_module_exec(properties, &property_name_offsets, &host_library, arena_offset));
            functions.push(python_module_registration(module, &host_library));
            let imports = python_imports(&methods, bool_exports, &host_library);
            let image = encode_x64_program_with_dependencies(&functions, &imports, abi, &data, &relocations, &host_dependencies)?;
            let entry_symbol = format!("PyInit_{module}");
            // 扩展后缀优先取解析服务冻结/探测的宿主 ABI 事实；无配置时才按平台
            // 规范名回落（`.pyd` / `.so`），产物身份差异由 ResolvedTarget::identity 承载。
            let extension = target
                .cpython_module_suffix()
                .map(|suffix| if suffix.starts_with('.') { suffix.to_string() } else { format!(".{suffix}") })
                .unwrap_or_else(|| if platform.is_windows() { ".pyd".to_string() } else { ".so".to_string() });
            (image, entry_symbol, extension)
        }
    };

    let executable = compile_shared_container(platform, &image)?;
    verify_shared_image(&executable, platform)?;

    // 宿主导出面按冻结顺序（与 methods 一致按公开名排序）携带源码名 + 公开名 +
    // 载体签名：签名只来自上面已闭合的 `HostMethod`，不在清单消费侧重新推导。
    let mut source_names: BTreeMap<String, String> = BTreeMap::new();
    for (instance, source_name) in submission.backend_plan.export_names() {
        source_names.insert(host_export_public_name(&submission.backend_plan, instance, host), source_name.clone());
    }
    let host_exports: Vec<MachineHostExport> = methods
        .iter()
        .map(|method| MachineHostExport {
            source_name: source_names.get(&method.name).cloned().unwrap_or_default(),
            public_name: method.name.clone(),
            parameters: method.parameters.iter().map(|kind| kind.tag().to_string()).collect(),
            returns: method.returns.tag().to_string(),
        })
        .collect();

    Ok(X64LoweredExtension {
        executable,
        entry_symbol,
        host_exports,
        artifact_extension,
        module_name: python_module,
    })
}

/// 机器码宿主扩展的命名合同：取实例的目标公开名。
///
/// - Node 有 `name:` / `case:` 注解：公开名就是注解解析名；
/// - Node 无注解：源码名自动 camelCase 投影（`move_zeroes` → `moveZeroes`）；
/// - CPython：源码名原样（Python 命名就是 snake_case，忽略注解改名）。
fn host_export_public_name(plan: &crate::TargetExecutionPlan, instance: &ItemInstanceId, host: HostModule) -> String {
    if host != HostModule::Python {
        if let Some(annotation) = plan.export_annotations().get(instance) {
            return annotation.clone();
        }
    }
    let source = plan.export_names().get(instance).cloned().unwrap_or_default();
    match host {
        HostModule::Node => compiler_types::naming::snake_case_to_camel_case(&source),
        HostModule::Python => source,
    }
}

/// 从冻结导出面构造类型化宿主方法事实；未闭合的载体在写出前拒绝。
///
/// 公开名按本目标命名合同投影；投影后两个不同源码名得到同一公开名（如 `foo_bar`
/// 与 `fooBar` 都落到 `fooBar`）在写出前 fail-closed，诊断同时列出两处源码名与实例身份。
fn frozen_host_methods(submission: &FragmentSubmission, target: &X64ProductTarget, reachable: &[ItemInstanceId]) -> Result<Vec<HostMethod>> {
    let host = target.host_module().unwrap_or(HostModule::Node);
    let limit = target.method_parameter_limit();
    let mut methods = Vec::new();
    let mut projected: BTreeMap<String, (String, ItemInstanceId)> = BTreeMap::new();
    for (instance, source_name) in submission.backend_plan.export_names() {
        let export_name = host_export_public_name(&submission.backend_plan, instance, host);
        if let Some((previous_source, previous_instance)) = projected.insert(export_name.clone(), (source_name.clone(), *instance)) {
            return Err(format!(
                "宿主扩展：源码名 `{previous_source}`（`{previous_instance:?}`）与 `{source_name}`（`{instance:?}`）投影为同一宿主公开名 `{export_name}`，拒绝产出互相覆盖的导出面"
            ));
        }
        if !reachable.contains(instance) {
            return Err(format!("宿主扩展：导出实例 `{instance:?}`（`{export_name}`）不在可达函数集中"));
        }
        let view = submission
            .backend_plan
            .get_function(instance)
            .ok_or_else(|| format!("宿主扩展：导出实例 `{instance:?}`（`{export_name}`）缺少函数体"))?;
        let function = &view.function;
        if function.param_types.len() > limit {
            return Err(format!(
                "宿主扩展方法 `{export_name}` 有 {} 个形参，超出 `{}` 的整数实参上限 {limit}",
                function.param_types.len(),
                target.name()
            ));
        }
        // 实参载体：完整 `i64`、宿主序列投影的 `[i64]` 堆数组，或 `utf8` 文本。
        // 数组按本 lane 的冻结表示逐元素编组，集合语义仍留在 std。
        let mut parameter_kinds = Vec::with_capacity(function.param_types.len());
        for parameter in &function.param_types {
            parameter_kinds.push(match parameter {
                SemanticType::Integer64 { signed: true } => HostParameterKind::Int64,
                SemanticType::Array(element) if matches!(element.as_ref(), SemanticType::Integer64 { signed: true }) => {
                    HostParameterKind::Int64Array
                }
                SemanticType::Utf8 => HostParameterKind::Utf8,
                other => {
                    return Err(format!(
                        "宿主边界只闭合完整 `i64`、`[i64]` 与 `utf8` 实参：`{export_name}` 的形参类型 `{other}` 没有无损宿主载体，拒绝写出"
                    ));
                }
            });
        }
        // CPython 边界的 `PyArg_ParseTuple` 闭合 64 位整数与 UTF-8 字符串：数组投影
        // 只在 Node-API 的序列编组里实现，不能把同一个导出悄悄降级成不完整的宿主载体。
        if target.host_module() == Some(HostModule::Python)
            && parameter_kinds.iter().any(|kind| matches!(kind, HostParameterKind::Int64Array))
        {
            return Err(format!("CPython 宿主边界只闭合 `i64` 与 `utf8` 实参：`{export_name}` 的导出面需要 Node-API 序列编组，拒绝写出"));
        }
        // 返回与实参共用同一符号合同：`u64` 的 `2^63..2^64-1` 段没有有符号
        // `i64` 载体可无损承载，在 BigInt 载体接入前与窄整数一样写出前拒绝。
        let returns = match &function.return_type {
            SemanticType::Integer64 { signed: true } => HostValueKind::Int64,
            SemanticType::Boolean => HostValueKind::Bool,
            SemanticType::Utf8 => HostValueKind::Utf8,
            SemanticType::Array(element) if matches!(element.as_ref(), SemanticType::Integer64 { signed: true }) => {
                // 序列返回只在 Node-API 侧物化；CPython 的列表载体尚未定案，显式拒绝。
                if target.host_module() == Some(HostModule::Python) {
                    return Err(format!("CPython 宿主边界尚未闭合 `[i64]` 返回：`{export_name}` 的序列载体只在 Node-API 侧实现，拒绝写出"));
                }
                HostValueKind::Int64Array
            }
            SemanticType::Unit => {
                if target.host_module() == Some(HostModule::Python) {
                    return Err(format!("CPython 宿主边界尚未闭合 `unit` 返回：`{export_name}` 拒绝写出"));
                }
                HostValueKind::Unit
            }
            other => {
                return Err(format!(
                    "宿主边界尚未闭合返回载体 `{other}`：`{export_name}` 只接受有符号 `i64`、`bool`、`utf8`、`[i64]` 或 `unit` 返回（其余嵌套序列与无符号 `u64` 尚无无损载体），拒绝写出"
                ));
            }
        };
        methods.push(HostMethod {
            name: export_name.clone(),
            symbol: sanitize_symbol(&export_name),
            parameters: parameter_kinds,
            returns,
        });
    }
    methods.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(methods)
}

/// 镜像符号名表：导出函数按目标公开名规约的符号，内部函数按实例身份。
fn extension_symbol_names(
    submission: &FragmentSubmission,
    target: &X64ProductTarget,
    reachable: &[ItemInstanceId],
    methods: &[HostMethod],
) -> Result<BTreeMap<ItemInstanceId, String>> {
    let host = target.host_module().unwrap_or(HostModule::Node);
    let mut names = BTreeMap::new();
    let mut used: BTreeMap<&str, &str> = BTreeMap::new();
    for method in methods {
        if let Some(previous) = used.insert(method.symbol.as_str(), method.name.as_str()) {
            return Err(format!("宿主扩展导出名 `{}` 与 `{previous}` 规约成同一镜像符号：拒绝产出互相覆盖的导出面", method.name));
        }
    }
    for instance in reachable {
        let name = match submission.backend_plan.export_names().get_key_value(instance) {
            Some((_, _)) => {
                let export_name = host_export_public_name(&submission.backend_plan, instance, host);
                let method = methods.iter().find(|method| method.name == export_name).expect("导出实例已构造方法事实");
                method.symbol.clone()
            }
            None => format!("__nyar_fn_{}", instance.index()),
        };
        if names.insert(*instance, name).is_some() {
            return Err(format!("宿主扩展：可达操作 `{instance:?}` 被重复登记"));
        }
    }
    Ok(names)
}

/// Windows 宿主导入模块名；Linux 的 `CallImport` 模块名必须为空。
fn host_import_library(target: &X64ProductTarget, platform: X64Platform) -> Result<String> {
    let Some(module) = target.host_module()
    else {
        return Err("目标不是宿主扩展合同".to_string());
    };
    if !platform.is_windows() {
        return Ok(String::new());
    }
    match module {
        HostModule::Node => Ok("node.exe".to_string()),
        HostModule::Python => target
            .cpython_import_library()
            .map(str::to_string)
            .ok_or_else(|| "Windows CPython 扩展必须提供宿主导入 DLL 名（例如 `python312.dll`）：版本相关名不能由编译器猜".to_string()),
    }
}

/// Node-API 导入面：注册与方法回调实际引用的符号。
fn node_imports(methods: &[HostMethod], bool_exports: bool, dll: &str) -> Vec<(String, String)> {
    let mut imports = vec![(dll.to_string(), "napi_set_named_property".to_string()), (dll.to_string(), "napi_create_bigint_int64".to_string())];
    if !methods.is_empty() {
        for symbol in ["napi_create_function", "napi_get_cb_info", "napi_get_value_bigint_int64", "napi_throw_type_error", "napi_throw_range_error"] {
            imports.push((dll.to_string(), symbol.to_string()));
        }
    }
    let takes_array = methods.iter().any(|method| method.parameters.iter().any(|kind| matches!(kind, HostParameterKind::Int64Array)));
    if takes_array {
        for symbol in ["napi_is_array", "napi_get_array_length", "napi_get_element", "napi_set_element", "napi_throw_error"] {
            imports.push((dll.to_string(), symbol.to_string()));
        }
    }
    if methods.iter().any(|method| method.parameters.iter().any(|kind| matches!(kind, HostParameterKind::Utf8))) {
        imports.push((dll.to_string(), "napi_get_value_string_utf8".to_string()));
        // 文本实参的临时副本分配失败走宿主 error 通道。
        if !takes_array {
            imports.push((dll.to_string(), "napi_throw_error".to_string()));
        }
    }
    if any_returns_utf8(methods) {
        imports.push((dll.to_string(), "napi_create_string_utf8".to_string()));
    }
    if any_returns_int64_array(methods) {
        for symbol in ["napi_create_array_with_length", "napi_set_element", "napi_throw_error"] {
            imports.push((dll.to_string(), symbol.to_string()));
        }
    }
    if bool_exports {
        imports.push((dll.to_string(), "napi_get_boolean".to_string()));
    }
    imports
}

/// CPython 导入面：多阶段初始化与方法回调实际引用的符号。
fn python_imports(methods: &[HostMethod], bool_exports: bool, dll: &str) -> Vec<(String, String)> {
    let mut imports = vec![
        (dll.to_string(), "PyModuleDef_Init".to_string()),
        (dll.to_string(), "PyLong_FromLongLong".to_string()),
        (dll.to_string(), "PyModule_AddObject".to_string()),
    ];
    if !methods.is_empty() {
        imports.push((dll.to_string(), "PyArg_ParseTuple".to_string()));
    }
    if any_returns_utf8(methods) {
        imports.push((dll.to_string(), "PyUnicode_FromStringAndSize".to_string()));
    }
    if methods.iter().any(|method| method.parameters.iter().any(|kind| matches!(kind, HostParameterKind::Utf8))) {
        imports.push((dll.to_string(), "PyUnicode_AsUTF8AndSize".to_string()));
    }
    if bool_exports {
        imports.push((dll.to_string(), "PyBool_FromLong".to_string()));
    }
    imports
}

/// 自检：产物必须是与平台身份一致的共享库镜像。
pub(super) fn verify_shared_image(executable: &[u8], platform: X64Platform) -> Result<()> {
    use crate::emitter_backend_machine::{MachineImageKind, classify_machine_image};
    let facts = classify_machine_image(executable).map_err(|error| format!("宿主扩展产物自检失败：{error}"))?;
    if !facts.shared {
        return Err("宿主扩展产物自检失败：镜像不是共享库".to_string());
    }
    let expected = if platform.is_windows() { MachineImageKind::Pe } else { MachineImageKind::ElfShared };
    if facts.kind != expected {
        return Err(format!("宿主扩展产物自检失败：平台 `{}` 期望 {expected:?}，实际 {facts:?}", platform.name()));
    }
    Ok(())
}
