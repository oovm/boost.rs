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

use crate::{Result, emitter_backend_machine::HostProperty};

use super::{
    HostMethod, HostModule, HostParameterKind, MachineTextCarrier, X64Platform, X64ProgramFunction, any_returns_bool, any_returns_int64_array,
    any_returns_utf8, append_text_arena,
    compile_shared_container, node_addon_registration, node_method_wrapper, node_module_data, python_method_wrapper,
    python_module_data, python_module_exec, python_module_registration, x64_program::encode_x64_program_with_dependencies,
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

/// CPython 模块名归一化的唯一实现已上收 `compiler_target::target_resolution`
/// （`cpython_module_name` / `ResolvedTarget::freeze_cpython_module_name`）：
/// 构建计划按工程名冻结一次，lowering 与清单只消费冻结事实，不再各自归一化。

/// 机器码交付的一条宿主导出：语言侧源码名与宿主合同投影后的公开名。
///
/// 命名策略是 `compiler_target` 宿主合同的显式面（[`compiler_target::HostExportNaming`]：
/// Node 注解优先 + camelCase 投影、CPython snake_case 投影忽略注解）；语言 lane 只
/// 提交源码名与注解事实。公开名是宿主装载面，源码名保留用于跨目标追溯；
/// 投影碰撞由合同在发射前结构化报错。本表是产物与清单共享的唯一导出面。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineHostExport {
    /// 语言侧冻结的源码声明名。
    pub source_name: String,
    /// 目标命名合同投影后的宿主公开名。
    pub public_name: String,
    /// 形参宿主载体标签（`i64` / `i64[]` / `utf8`，按形参顺序）；由冻结签名导出，
    /// 供执行清单的宿主消费者区分整数、序列与文本，不在消费侧重新猜测。
    pub parameters: Vec<String>,
    /// 形参声明名（按形参顺序，与 `parameters` 等长）；消费侧按名绑定用例字段，
    /// 不依赖 JSON 对象键序。空列表表示该导出没有形参或名称事实不可用。
    pub parameter_names: Vec<String>,
    /// 返回宿主载体标签（`i64` / `bool` / `utf8` / `i64[]` / `unit`）。
    pub returns: String,
}

/// 宿主扩展的目标私有输入：已降低的 LIR 程序 + 冻结的宿主边界事实。
///
/// 装配服务（宿主包装体编组 + 编码 + 共享库容器 + 自检）只接受这个形状，不再
/// 接触任何跨目标共享 IR 形状。文本载体描述按需显式提供（[`MachineTextCarrier`]）：
/// 导出面含 `utf8` 载体时装配必须携带，不含时不会被消费；桥不默认构造、
/// 也不向消费端读回该事实。
pub struct X64ExtensionProgramInput {
    platform: X64Platform,
    host: HostModule,
    host_library: String,
    host_dependencies: Vec<String>,
    functions: Vec<X64ProgramFunction>,
    methods: Vec<HostMethod>,
    host_exports: Vec<MachineHostExport>,
    module_name: Option<String>,
    artifact_extension: String,
    text_carrier: Option<MachineTextCarrier>,
}

impl X64ExtensionProgramInput {
    /// 语言适配器构造目标私有宿主扩展输入：平台、宿主合同、按需文本载体描述与产物身份一次冻结。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        platform: X64Platform,
        host: HostModule,
        host_library: String,
        host_dependencies: Vec<String>,
        functions: Vec<X64ProgramFunction>,
        methods: Vec<HostMethod>,
        host_exports: Vec<MachineHostExport>,
        module_name: Option<String>,
        artifact_extension: String,
        text_carrier: Option<MachineTextCarrier>,
    ) -> Self {
        Self { platform, host, host_library, host_dependencies, functions, methods, host_exports, module_name, artifact_extension, text_carrier }
    }

    /// 目标平台身份（容器与调用约定一次冻结）。
    pub fn platform(&self) -> X64Platform {
        self.platform
    }

    /// 宿主模块合同（Node-API / CPython）。
    pub fn host_module(&self) -> HostModule {
        self.host
    }
}

/// 目标私有宿主扩展输入 → 共享库镜像（宿主包装体编组 + 编码 + 容器 + 自检）。
pub fn assemble_x64_extension_image(input: &X64ExtensionProgramInput) -> Result<X64LoweredExtension> {
    let platform = input.platform;
    let abi = platform.abi();
    let properties: &[HostProperty] = &[];
    let mut functions = input.functions.clone();
    // 文本载体描述按需校验：导出面含 utf8 载体却没有显式描述时，在编码前
    // 结构化失败；桥不默认构造布局事实，也不允许静默省略。
    if input.methods.iter().any(HostMethod::uses_text_carrier) && input.text_carrier.is_none() {
        return Err("宿主导出面含 utf8 载体但未提供显式文本载体描述：语言适配器必须按需提交载体布局事实，桥不默认构造".to_string());
    }
    let text_carrier = input.text_carrier;

    let (image, entry_symbol) = match input.host {
        HostModule::Node => {
            let (node_data, property_name_offsets, method_name_offsets, error_message_offset, range_error_offset, allocation_error_offset) =
                node_module_data(properties, &input.methods);
            let mut data = node_data;
            let arena_offset = Some(append_text_arena(&mut data));
            let bool_exports = any_returns_bool(properties, &input.methods);
            for method in &input.methods {
                functions.push(node_method_wrapper(
                    method,
                    error_message_offset,
                    range_error_offset,
                    allocation_error_offset,
                    platform,
                    &input.host_library,
                    arena_offset,
                    text_carrier,
                ));
            }
            functions.push(node_addon_registration(
                properties,
                &property_name_offsets,
                input.methods.as_slice(),
                &method_name_offsets,
                platform,
                &input.host_library,
                arena_offset,
            ));
            let imports = node_imports(&input.methods, bool_exports, &input.host_library);
            let image = encode_x64_program_with_dependencies(&functions, &imports, abi, &data, &[], &input.host_dependencies)?;
            (image, "napi_register_module_v1".to_string())
        }
        HostModule::Python => {
            let module = input
                .module_name
                .as_deref()
                .ok_or("CPython 宿主扩展缺少冻结模块名：构建计划必须经 `freeze_cpython_module_name` 装配后再 lowering")?;
            let (definition, relocations, property_name_offsets, format_offsets) = python_module_data(module, properties, &input.methods);
            let mut data = definition;
            let arena_offset = Some(append_text_arena(&mut data));
            let bool_exports = any_returns_bool(properties, &input.methods);
            for (method, format_offset) in input.methods.iter().zip(format_offsets) {
                functions.push(python_method_wrapper(method, format_offset, &input.host_library, arena_offset, platform, text_carrier));
            }
            functions.push(python_module_exec(properties, &property_name_offsets, &input.host_library, arena_offset));
            functions.push(python_module_registration(module, &input.host_library));
            let imports = python_imports(&input.methods, bool_exports, &input.host_library);
            let image = encode_x64_program_with_dependencies(&functions, &imports, abi, &data, &relocations, &input.host_dependencies)?;
            (image, format!("PyInit_{module}"))
        }
    };

    let executable = compile_shared_container(platform, &image)?;
    verify_shared_image(&executable, platform)?;

    Ok(X64LoweredExtension {
        executable,
        entry_symbol,
        host_exports: input.host_exports.clone(),
        artifact_extension: input.artifact_extension.clone(),
        module_name: input.module_name.clone(),
    })
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

#[cfg(test)]
mod tests {
    use super::{super::HostValueKind, *};

    /// 导出面含 utf8 载体却未提供显式文本载体描述：编码前结构化失败，
    /// 桥不默认构造布局事实。
    #[test]
    fn utf8_export_without_carrier_description_fails_before_encoding() {
        let method = HostMethod {
            name: "echo".to_string(),
            symbol: "echo".to_string(),
            parameters: vec![HostParameterKind::Utf8],
            returns: HostValueKind::Int64,
        };
        let input = X64ExtensionProgramInput::new(
            X64Platform::linux_x64(),
            HostModule::Node,
            String::new(),
            Vec::new(),
            Vec::new(),
            vec![method],
            Vec::new(),
            None,
            ".node".to_string(),
            None,
        );
        let error = assemble_x64_extension_image(&input).expect_err("utf8 载体必须显式提供文本载体描述");
        assert!(error.contains("文本载体描述"), "错误必须指名缺失的载体描述，got: {error}");
    }

    /// 不含 utf8 载体的导出面不需要载体描述：输入按需接收，不强制携带。
    #[test]
    fn integer_only_exports_do_not_require_carrier_description() {
        let method = HostMethod {
            name: "add".to_string(),
            symbol: "add".to_string(),
            parameters: vec![HostParameterKind::Int64],
            returns: HostValueKind::Int64,
        };
        assert!(!method.uses_text_carrier(), "纯整数方法不得被判定为需要文本载体");
    }
}
