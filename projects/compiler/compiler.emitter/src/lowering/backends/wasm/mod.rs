//! Wasm / WASI 目标的 executable 物理编码与 GC 布局。
//!
//! `mir/` 是尚未迁移的历史目录名，不是另一份语言 MIR。
//! 正式入口只能消费语言侧从 MIR 直出结算的目标私有输入（[`WasmProgramInput`]）；
//! 宿主包装只属于产物层，不得由调用摘要生成可执行语义。
//! `suspend` 保留显式参数驱动的底层控制流编码。

pub mod input;
pub(crate) mod abi;
#[doc(hidden)]
pub mod cabi;
mod gc;
// 公共 `ExternalImportLink` 分类器只服务 `shared::interop` 的裁决测试；
// 编码层消费的是 `mir/imports.rs` 的私有版分类器。
#[cfg(test)]
pub(crate) mod interop;
pub(crate) mod mir;
mod sections;

pub(crate) use cabi::{
    CABI_HEAP_DEFAULT_BASE, CABI_HEAP_GLOBAL_INDEX, LINEAR_HEAP_MIN_BASE, align_up_u32, cabi_heap_base_after_data, cabi_heap_global_section,
    memory_min_pages_for_heap_base, wasm_cabi_realloc_bump_body,
};

pub(crate) use sections::{
    append_wasm_code_bodies, append_wasm_exports, append_wasm_function_decls, append_wasm_i32_globals, append_wasm_types, code_section_bytes,
    count_wasm_function_decls, count_wasm_function_imports, count_wasm_globals, count_wasm_types, data_section_bytes, decode_uleb128,
    encode_name, encode_sleb128_i32, encode_sleb128_i64, encode_uleb128, export_section_bytes, function_section_bytes, global_section_bytes,
    global_section_with_i32_inits, import_section_bytes, insert_wasm_section, memory_section_bytes, type_section_bytes, wasm_function_body,
    wasm_function_type,
};

use crate::{
    Result,
    emitter_backend_wasi::{WasiPreview, WasmBinaryModule, WasmLibraryExport, WasmPackageKind, WasmSection, WasmTestEntryExport},
};
use compiler_target::HostProjectionBoundary;

use input::WasmModuleInput;

/// 该宿主边界上程序入口的冻结导出名（lane 保留词汇）。
///
/// 入口名只由宿主边界决定；宿主绑定生成器必须消费这个名字，不得在运行时按
/// `main` / `_start` 再猜一次——猜错时启动壳会静默不调用入口并退出 0。
pub fn host_entry_export_name(host_boundary: HostProjectionBoundary) -> Result<&'static str> {
    match host_boundary {
        HostProjectionBoundary::WasmJsGlue => Ok("main"),
        HostProjectionBoundary::WasiComponent => Ok("_start"),
        other => Err(format!("WASM host boundary is not supported: {other:?}")),
    }
}

/// Wasm 目标私有输入：结算完成的模块事实与装配消费的交付事实。
///
/// 合同门（文本 ABI、存储身份、导出面与包型判据）与交付事实（有效包型、
/// WASI command 判定、库导出签名、测试入口映射、宿主入口导出名）在语言侧
/// 纵链第一段一次结算；装配服务 [`assemble_wasm_module`] 只接受这个形状，
/// 做段编码、自定义段装配与模块形状自检，不再接触跨目标共享程序。
pub struct WasmProgramInput {
    module_input: WasmModuleInput,
    entry_export_name: String,
    host_boundary: HostProjectionBoundary,
    wasi_preview: WasiPreview,
    wasm_package_kind: WasmPackageKind,
    package_as_wasi_command: bool,
    library_wasm_exports: Vec<WasmLibraryExport>,
    test_entry_exports: Vec<WasmTestEntryExport>,
    host_entry_export: Option<String>,
}

impl WasmProgramInput {
    /// 冻结目标私有输入与交付事实为一个装配包。
    pub fn new(
        module_input: WasmModuleInput,
        entry_export_name: String,
        host_boundary: HostProjectionBoundary,
        wasi_preview: WasiPreview,
        wasm_package_kind: WasmPackageKind,
        package_as_wasi_command: bool,
        library_wasm_exports: Vec<WasmLibraryExport>,
        test_entry_exports: Vec<WasmTestEntryExport>,
        host_entry_export: Option<String>,
    ) -> Self {
        Self {
            module_input,
            entry_export_name,
            host_boundary,
            wasi_preview,
            wasm_package_kind,
            package_as_wasi_command,
            library_wasm_exports,
            test_entry_exports,
            host_entry_export,
        }
    }

    /// 结算完成的 wasm 模块私有输入。
    pub fn module_input(&self) -> &WasmModuleInput {
        &self.module_input
    }

    /// 宿主边界冻结的入口导出名。
    pub fn entry_export_name(&self) -> &str {
        &self.entry_export_name
    }

    /// 宿主投影边界。
    pub fn host_boundary(&self) -> HostProjectionBoundary {
        self.host_boundary
    }

    /// WASI 打包链选择。
    pub fn wasi_preview(&self) -> WasiPreview {
        self.wasi_preview
    }

    /// 有效包型（binary / library）。
    pub fn wasm_package_kind(&self) -> WasmPackageKind {
        self.wasm_package_kind
    }

    /// 是否按 WASI command world 打包。
    pub fn package_as_wasi_command(&self) -> bool {
        self.package_as_wasi_command
    }

    /// 库导出签名事实。
    pub fn library_wasm_exports(&self) -> &[WasmLibraryExport] {
        &self.library_wasm_exports
    }

    /// 测试包逐入口映射。
    pub fn test_entry_exports(&self) -> &[WasmTestEntryExport] {
        &self.test_entry_exports
    }

    /// 启动壳必须调用的冻结入口导出名。
    pub fn host_entry_export(&self) -> Option<&str> {
        self.host_entry_export.as_deref()
    }
}

/// Wasm 目标私有输入 → 模块产物：段编码、自定义段装配与模块形状自检。
///
/// 签名里没有跨目标共享程序：宿主包装、WAT sidecar 与 WASI 打包都在同一份
/// 目标私有输入上完成。
pub fn assemble_wasm_module(input: &WasmProgramInput) -> Result<(WasmBinaryModule, Vec<(String, String)>)> {
    let (mut module, imports) = mir::lower_fragment_mir_to_wasm_module_for(
        &input.module_input,
        &input.entry_export_name,
        input.host_boundary,
        input.wasi_preview,
        input.wasm_package_kind,
    )?;
    prepend_nyar_custom_sections(&mut module, &input.module_input);
    append_host_provider_section(&mut module, &input.module_input, input.host_boundary);
    mir::singleton::append_singleton_metadata_sections(&mut module, &input.module_input);
    mir::singleton::augment_wasm_with_singleton_accessors(&mut module, &input.module_input)
        .map_err(|error| format!("singleton contract failed [{}] {} at {}: {}", error.code, error.function, error.location, error.detail))?;
    mir::augment_wasm_with_value_aggregate_metadata(&mut module, &input.module_input);
    if module.version == 0 || module.sections.is_empty() {
        return Err("WASM 装配：目标私有输入不是有效模块（版本为 0 或没有段），拒绝装配".to_string());
    }
    Ok((module, imports))
}

/// 把适配器提交的宿主导入 provider 写入 `nyar.host_providers` 自定义段。
///
/// 仅 JS glue 边界有意义（Node 启动壳在生成期读取该段注入实现与辅助 JS）；
/// 段内容是事实的机械序列化，不含任何 wasm 侧语言语义。
fn append_host_provider_section(module: &mut WasmBinaryModule, input: &WasmModuleInput, boundary: HostProjectionBoundary) {
    if !matches!(boundary, HostProjectionBoundary::WasmJsGlue) || input.host_import_providers.is_empty() {
        return;
    }
    let providers: Vec<serde_json::Value> = input
        .host_import_providers
        .iter()
        .map(|provider| {
            serde_json::json!({
                "module": provider.module,
                "field": provider.field,
                "implementation": provider.implementation,
                "helpers": provider.helpers,
            })
        })
        .collect();
    let payload = serde_json::to_vec(&serde_json::json!({ "providers": providers })).expect("provider 段必须可序列化");
    module.sections.push(WasmSection { id: 0, name: Some("nyar.host_providers".to_string()), bytes: payload });
}

fn prepend_nyar_custom_sections(module: &mut WasmBinaryModule, input: &WasmModuleInput) {
    let mut customs = vec![
        ("nyar.module".to_string(), input.module_name.as_bytes().to_vec()),
        ("nyar.fragment".to_string(), input.fragment_id.as_bytes().to_vec()),
    ];
    for operation in &input.exported_operations {
        customs.push(("nyar.export".to_string(), operation.to_string().into_bytes()));
    }
    for capability in &input.required_capabilities {
        customs.push(("nyar.capability".to_string(), capability.clone().into_bytes()));
    }
    if let Some(entry) = input.entry_operation {
        customs.push(("nyar.entry".to_string(), entry.to_string().into_bytes()));
    }
    customs.push(("nyar.theory.rules".to_string(), input.theory_rules.to_string().into_bytes()));
    customs.push(("nyar.theory.equations".to_string(), input.theory_equations.to_string().into_bytes()));

    let mut prepended = customs.into_iter().map(|(name, bytes)| WasmSection { id: 0, name: Some(name), bytes }).collect::<Vec<_>>();
    prepended.append(&mut module.sections);
    module.sections = prepended;
}
