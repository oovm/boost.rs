//! Wasm / WASI 目标的 executable 物理编码与 GC 布局。
//!
//! `mir/` 是尚未迁移的历史目录名，不是另一份语言 MIR。
//! 正式入口只能消费 Compiler 的函数体；宿主包装只属于产物层，
//! 不得由调用摘要生成可执行语义。`suspend` 保留显式参数驱动的底层控制流编码。

pub(crate) mod abi;
#[doc(hidden)]
pub mod cabi;
mod gc;
pub(crate) mod input;
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
pub(crate) use gc::{WASM_GC_ANYREF, wasm_gc_array_type, wasm_gc_field_type_byte, wasm_gc_struct_type};
pub(crate) use sections::{
    append_wasm_code_bodies, append_wasm_exports, append_wasm_function_decls, append_wasm_i32_globals, append_wasm_types, code_section_bytes,
    count_wasm_function_decls, count_wasm_function_imports, count_wasm_globals, count_wasm_types, data_section_bytes, decode_uleb128,
    encode_name, encode_sleb128_i32, encode_sleb128_i64, encode_uleb128, export_section_bytes, function_section_bytes, global_section_bytes,
    global_section_with_i32_inits, import_section_bytes, insert_wasm_section, memory_section_bytes, type_section_bytes, wasm_function_body,
    wasm_function_type,
};

use crate::{
    EmitterError, FragmentSubmission, Result,
    backend_plan_views::ExecutableConstant,
    emitter_backend_wasi::{WasiPreview, WasmBinaryModule, WasmPackageKind, WasmSection},
};
use compiler_target::{HostProjectionBoundary, SemanticType};

use crate::lowering::features::physical_contract::PhysicalTextBoundary;

/// 将片段降低为指定宿主边界的 wasm 模块（默认 WASI Preview2 打包链，集成测试专用）。
///
/// Lower a fragment for a wasm host boundary (default WASI Preview2 package train).
#[doc(hidden)]
pub fn lower_fragment_to_wasm_module(
    submission: &FragmentSubmission,
    host_boundary: HostProjectionBoundary,
) -> Result<(WasmBinaryModule, Vec<(String, String)>)> {
    lower_fragment_to_wasm_module_for(submission, host_boundary, WasiPreview::Preview2, WasmPackageKind::Binary)
}

/// 该宿主边界上程序入口的冻结导出名（lane 保留词汇）。
///
/// 入口名只由宿主边界决定；宿主绑定生成器必须消费这个名字，不得在运行时按
/// `main` / `_start` 再猜一次——猜错时启动壳会静默不调用入口并退出 0。
pub(crate) fn host_entry_export_name(host_boundary: HostProjectionBoundary) -> Result<&'static str> {
    match host_boundary {
        HostProjectionBoundary::WasmJsGlue => Ok("main"),
        HostProjectionBoundary::WasiComponent => Ok("_start"),
        other => Err(format!("WASM host boundary is not supported: {other:?}")),
    }
}

/// Wasm 目标私有输入：已编码模块与导出/导入事实。
///
/// 计划事实（语义/物理合同门、导出面判据、入口符号、各段编码、singleton 与
/// 值聚合元数据）在 [`build_wasm_program_input`] 里一次结算；装配服务
/// [`assemble_wasm_module`] 只接受这个形状，不再接触跨目标的
/// `FragmentSubmission` / `ExecutableFunction` / `InstructionKind` / `Terminator`。
pub(crate) struct WasmProgramInput {
    module: WasmBinaryModule,
    imports: Vec<(String, String)>,
}

impl WasmProgramInput {
    /// 已编码的 wasm 模块。
    pub(crate) fn module(&self) -> &WasmBinaryModule {
        &self.module
    }

    /// 导出/导入事实（`(module, name)`）。
    pub(crate) fn imports(&self) -> &[(String, String)] {
        &self.imports
    }
}

/// 将片段降低为显式 WASI 打包链的 wasm 模块（集成测试专用）。
///
/// Lower a fragment for a wasm host boundary with an explicit WASI package train.
///
/// 只编码 Compiler 提交的 executable；调用边摘要与宿主包装不得替代函数体。
#[doc(hidden)]
pub fn lower_fragment_to_wasm_module_for(
    submission: &FragmentSubmission,
    host_boundary: HostProjectionBoundary,
    wasi_preview: WasiPreview,
    wasm_package_kind: WasmPackageKind,
) -> Result<(WasmBinaryModule, Vec<(String, String)>)> {
    assemble_wasm_module(&build_wasm_program_input(submission, host_boundary, wasi_preview, wasm_package_kind)?)
}

/// 计划事实 → Wasm 目标私有输入（不装配边界壳、不做模块自检）。
pub(crate) fn build_wasm_program_input(
    submission: &FragmentSubmission,
    host_boundary: HostProjectionBoundary,
    wasi_preview: WasiPreview,
    wasm_package_kind: WasmPackageKind,
) -> Result<WasmProgramInput> {
    let boundary_entry_name = host_entry_export_name(host_boundary)?;
    crate::lowering::features::semantic_mir_contract::validate_submission(submission)
        .map_err(|error| format!("semantic MIR contract failed [{}] {} at {}: {}", error.code, error.function, error.location, error.detail))?;
    let physical_backend = match host_boundary {
        HostProjectionBoundary::WasmJsGlue => crate::lowering::features::physical_contract::PhysicalBackend::WasmJsGlue,
        HostProjectionBoundary::WasiComponent => crate::lowering::features::physical_contract::PhysicalBackend::WasiComponent,
        _ => crate::lowering::features::physical_contract::PhysicalBackend::WasmCore,
    };
    crate::lowering::features::physical_contract::validate_physical_submission(submission, physical_backend)
        .map_err(|error| format!("physical contract failed [{}] {} at {}: {}", error.code, error.function, error.location, error.detail))?;
    validate_text_encoding_projection(submission, host_boundary)?;
    let has_executable = !submission.backend_plan.instances().is_empty();
    if !has_executable {
        return Err(format!(
            "WASM requires Compiler-owned executable functions; call-edge replay and empty entry synthesis are not valid inputs"
        ));
    }
    let executable = &submission.backend_plan;
    // 导出面判据与 OVM 共用同一派生点：声明的 `[export]` 名 ∪ 程序入口，可达闭包不算。
    for operation in crate::lowering::shared::export_surface::export_surface(executable) {
        if executable.get_function(&operation).is_none() {
            return Err(format!("WASM callable `{operation}` has no Compiler-owned executable body"));
        }
    }
    match wasm_package_kind {
        WasmPackageKind::Library => {
            if submission.backend_plan.export_names().is_empty() {
                return Err("library wasm package requires at least one `[export]` symbol".to_string());
            }
        }
        WasmPackageKind::Binary => {
            let entry = submission
                .backend_plan
                .entry_operation()
                .ok_or_else(|| "WASM binary package requires a Compiler-resolved entry".to_string())?;
            // 宿主入口名是 lane 保留符号：其它导出的公开名不得占用，否则入口导出与
            // `[export]` 导出会写进同一个名字，产出重复导出名的非法模块。
            if let Some(instance) = submission.backend_plan.export_names().keys().copied().find(|instance| {
                *instance != entry
                    && submission
                        .backend_plan
                        .export_public_name(instance)
                        .is_some_and(|name| name.as_str() == boundary_entry_name)
            }) {
                return Err(format!(
                    "WASM binary package: `[export]` 实例 `{instance}` 的公开名与宿主入口符号 `{boundary_entry_name}` 冲突"
                ));
            }
            // 导出公开名是宿主装载身份，必须唯一；重复会让模块写出重复导出名。
            // 库模式在构建 `nyar.library_invoke` 时以同一规则拒绝重复，这里覆盖二进制包。
            let mut public_names = std::collections::BTreeSet::new();
            for instance in submission.backend_plan.export_names().keys() {
                let name = submission
                    .backend_plan
                    .export_public_name(instance)
                    .ok_or_else(|| format!("WASM binary package: 导出实例 `{instance}` 缺少公开名"))?;
                if !public_names.insert(name.clone()) {
                    return Err(format!("WASM binary package: 导出公开名 `{name}` 重复"));
                }
            }
        }
    }
    // 入口导出名只由宿主边界的冻结词汇决定（`main` / `_start`）。`[export]` 公开名
    // 属于另一个命名面，不能借给入口函数：否则入口会冒用导出名，或与同名导出一起
    // 产出重复导出名的非法模块。入口命名不参与宿主模式推断。
    let entry_export_name = boundary_entry_name;
    // 存储类选择的唯一闸门：编码前拒绝无身份名义类型残差。形态判断发生在
    // `storage_for_type` / `wasm_param_value_type` / GC 字段字节之前；本门留在
    // lane 装配点，编码层（`mir/`）不再接触公共提交。
    validate_wasm_storage_identities(submission)?;
    // 公共 IR 在此一次结算为 wasm 私有输入；`mir/` 全部子模块只消费私有词汇。
    let input = input::settle_from_submission(submission)?;
    let (mut module, imports) =
        mir::lower_fragment_mir_to_wasm_module_for(&input, entry_export_name, host_boundary, wasi_preview, wasm_package_kind)?;

    prepend_nyar_custom_sections(&mut module, &input);
    mir::singleton::append_singleton_metadata_sections(&mut module, &input);
    mir::singleton::augment_wasm_with_singleton_accessors(&mut module, &input)
        .map_err(|error| format!("singleton contract failed [{}] {} at {}: {}", error.code, error.function, error.location, error.detail))?;
    mir::augment_wasm_with_value_aggregate_metadata(&mut module, &input);
    Ok(WasmProgramInput { module, imports })
}

/// 默认宿主边界（WasmJsGlue / Preview2 / Binary）的完整降低：只做私有结算与
/// 编码门，不装配边界壳。供 `lowering` 层的测试与旧入口使用。
pub(crate) fn lower_fragment_mir_to_wasm_module(
    submission: &FragmentSubmission,
    export_name: &str,
) -> Result<(WasmBinaryModule, Vec<(String, String)>)> {
    validate_wasm_storage_identities(submission)?;
    let input = input::settle_from_submission(submission)?;
    mir::lower_fragment_mir_to_wasm_module_for(
        &input,
        export_name,
        HostProjectionBoundary::WasmJsGlue,
        crate::emitter_backend_wasi::WasiPreview::Preview2,
        crate::emitter_backend_wasi::WasmPackageKind::Binary,
    )
}

/// 存储类身份门：无 declaration / `TypeId` 的名义类型残差在编码前结构化拒绝。
fn validate_wasm_storage_identities(submission: &FragmentSubmission) -> Result<()> {
    crate::lowering::shared::executable::validate_storage_identities(submission).map_err(|error| {
        let owner = if error.function.is_empty() { error.site.clone() } else { format!("{} at {}", error.function, error.site) };
        format!("WASM pre-emission verifier: {owner} 携带无身份名义类型 `{}`；declaration / `TypeId` 缺失时不得按短名解析存储类", error.ty)
    })
}

/// Wasm 目标私有输入 → 模块产物：只做模块形状自检。
///
/// 签名里没有跨目标共享程序：宿主包装、WAT sidecar 与 WASI 打包都在同一份
/// 目标私有输入上完成。
pub(crate) fn assemble_wasm_module(input: &WasmProgramInput) -> Result<(WasmBinaryModule, Vec<(String, String)>)> {
    if input.module().version == 0 || input.module().sections.is_empty() {
        return Err("WASM 装配：目标私有输入不是有效模块（版本为 0 或没有段），拒绝装配".to_string());
    }
    Ok((input.module().clone(), input.imports().to_vec()))
}

/// 文本 ABI 站点的准入守卫：编码身份只由声明的 `SemanticType` 与 `host_boundary`
/// 决定（`physical_contract::declared_text_projection`），不看函数名或包名。
/// 只有直接位于 ABI 位置的 `utf16` 才有冻结契约；嵌套在聚合中的 `utf16` 与
/// `utf16` 字面量没有声明位置，仍按原错误拒绝。
#[doc(hidden)]
pub fn validate_text_encoding_projection(submission: &FragmentSubmission, host_boundary: HostProjectionBoundary) -> Result<()> {
    let executable = &submission.backend_plan;
    let text_abi_boundary = host_text_abi_boundary(host_boundary);
    for operation in executable.instances() {
        let Some(view) = executable.get_function(&operation)
        else {
            continue;
        };
        let function = &view.function;
        let mut sites = vec![("return type".to_string(), &function.return_type)];
        sites.extend(function.param_types.iter().enumerate().map(|(index, ty)| (format!("parameter {index}"), ty)));
        sites.extend(function.value_types.iter().map(|(value, ty)| (format!("SSA value {}", value.0), ty)));
        for (site, ty) in sites {
            if text_abi_site_denied(ty, text_abi_boundary) {
                return Err(format!(
                    "WASM pre-emission verifier: {} function `{}` {site} requires an explicit UTF-16 ABI contract; {:?} must not use the UTF-8 carrier",
                    boundary_name(host_boundary),
                    function.symbol,
                    host_boundary,
                ));
            }
        }
        for block in &function.blocks {
            for (index, instruction) in block.instructions.iter().enumerate() {
                if let crate::contracts::InstructionKind::LoadConstant { constant: ExecutableConstant::Utf16(_), .. } = &instruction.kind {
                    return Err(format!(
                        "WASM pre-emission verifier: {} function `{}` block {} instruction {index} contains a UTF-16 literal without an explicit UTF-16 ABI contract",
                        boundary_name(host_boundary),
                        function.symbol,
                        block.id.0,
                    ));
                }
            }
        }
    }
    Ok(())
}

fn host_text_abi_boundary(host_boundary: HostProjectionBoundary) -> Option<PhysicalTextBoundary> {
    match host_boundary {
        HostProjectionBoundary::WasmJsGlue => Some(PhysicalTextBoundary::WasmJsGlue),
        HostProjectionBoundary::WasiComponent => Some(PhysicalTextBoundary::WasiComponent),
        _ => None,
    }
}

fn text_abi_site_denied(ty: &SemanticType, boundary: Option<PhysicalTextBoundary>) -> bool {
    if !type_contains_utf16(ty) {
        return false;
    }
    match ty {
        // 只有直接位于 ABI 位置的 `utf16` 才查冻结事实；聚合内的 `utf16`
        // 没有独立的 ABI 位置，保持原有拒绝。
        SemanticType::Utf16 => match boundary {
            Some(boundary) => crate::lowering::features::physical_contract::declared_text_projection(boundary, ty).is_none(),
            None => true,
        },
        _ => true,
    }
}

fn type_contains_utf16(ty: &SemanticType) -> bool {
    match ty {
        SemanticType::Utf16 => true,
        SemanticType::Apply(base, arguments) => type_contains_utf16(base) || arguments.iter().any(type_contains_utf16),
        SemanticType::Function(function) => type_contains_utf16(&function.return_type) || function.params.iter().any(type_contains_utf16),
        SemanticType::Tuple(elements) | SemanticType::Union(elements) => elements.iter().any(type_contains_utf16),
        SemanticType::Array(element) | SemanticType::FixedArray { element, .. } => type_contains_utf16(element),
        SemanticType::TraitObject(object) => object.type_arguments.iter().any(type_contains_utf16),
        _ => false,
    }
}

fn boundary_name(host_boundary: HostProjectionBoundary) -> &'static str {
    match host_boundary {
        HostProjectionBoundary::WasmJsGlue => "WasmJsGlue",
        HostProjectionBoundary::WasiComponent => "WasiComponent",
        _ => "WASM",
    }
}

fn prepend_nyar_custom_sections(module: &mut WasmBinaryModule, input: &input::WasmModuleInput) {
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
