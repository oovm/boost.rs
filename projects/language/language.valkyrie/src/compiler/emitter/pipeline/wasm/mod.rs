//! Wasm lane 纵链第一段：从 lane 上下文直出目标私有输入并一次结算交付事实。
//!
//! 本模块是 wasm 合同门的唯一所在地：文本 ABI、可执行体存在性、导出面与
//! 包型判据、存储类身份，全部在语言侧对目标私有输入执行；结算后的
//! [`WasmProgramInput`] 交给 emitter 装配服务编码，公共 IR 不再出现在链上。
//! 门文案与旧公共路径逐字一致（`WASM pre-emission verifier: ...` 等），
//! 仅失败闭合的阶段归因由 `TargetPlanError` 携带。

use std::collections::{BTreeMap, BTreeSet};

use compiler_emitter::{
    emitter_backend_wasi::{WasiPreview, WasmLibraryExport, WasmPackageKind, WasmTestEntryExport},
    emitter_backend_wasm::{
        PhysicalTextBoundary, WasmCallableKey, WasmConstant, WasmModuleInput, WasmOperation, WasmProgramInput, WasmTypeFact,
        canonical_parameter_identities, declared_text_projection, host_entry_export_name, wasm_export_abi,
    },
};
use compiler_target::{ArtifactPurpose, HostProjectionBoundary};

use crate::compiler::emitter::pipeline::lane::LaneContext;
use crate::compiler::emitter::pipeline::{AdapterStage, TargetPlanError};
use crate::compiler::emitter::pipeline::wasm::lowerer::build_module_input;

pub(crate) mod lowerer;

/// 适配的失败闭合：链接事实与语言合同无法无损投影时按阶段归因。
fn unsound(stage: AdapterStage, instance: Option<compiler_types::ItemInstanceId>, symbol: Option<String>, reason: String) -> TargetPlanError {
    TargetPlanError::Unsound { stage, instance, symbol, reason }
}

/// 从 lane 上下文与交付合同结算 wasm 目标私有输入。
///
/// 顺序与旧公共路径一致：入口名 → 私有结算 → 文本 ABI 门 → 可执行体存在性 →
/// 导出面判据 → 包型门 → 存储类身份门 → 交付事实（有效包型、WASI command、
/// 库导出签名、测试入口映射、宿主入口导出名）。
pub(crate) fn build_wasm_program_input(
    ctx: &LaneContext,
    theory_bundle: &compiler_optimize::TheoryBundle,
    host_boundary: HostProjectionBoundary,
    wasi_preview: WasiPreview,
    wasm_package_kind: WasmPackageKind,
    artifact_purpose: ArtifactPurpose,
) -> Result<WasmProgramInput, TargetPlanError> {
    // 入口导出名只由宿主边界的冻结词汇决定（`main` / `_start`）。`[export]` 公开名
    // 属于另一个命名面，不能借给入口函数：否则入口会冒用导出名，或与同名导出一起
    // 产出重复导出名的非法模块。入口命名不参与宿主模式推断。
    let entry_export_name = host_entry_export_name(host_boundary)
        .map_err(|error| unsound(AdapterStage::Link, None, None, error))?
        .to_string();
    // 公共 IR 不再进入本链：语言 MIR 在此一次结算为 wasm 私有输入，
    // `mir/` 编码层只消费私有词汇。
    let module_input = build_module_input(ctx, theory_bundle)
        .map_err(|reason| unsound(AdapterStage::Representation, None, None, reason))?;

    // 测试包可能一并编译只承载程序入口、没有冻结 `[test]` 导出的辅助分区
    // （例如同包的 `[main]` 片段）。这类分区按二进制形状编译，绝不产出测试
    // 运行合同；只有承载测试导出的分区保持库形状。
    let effective_package_kind = if artifact_purpose == ArtifactPurpose::TestBundle
        && module_input.export_names.is_empty()
        && module_input.entry_operation.is_some()
    {
        WasmPackageKind::Binary
    }
    else {
        wasm_package_kind
    };

    validate_text_encoding_projection(&module_input, host_boundary)
        .map_err(|reason| unsound(AdapterStage::Representation, None, None, reason))?;
    if module_input.functions.is_empty() {
        return Err(unsound(
            AdapterStage::Function,
            None,
            None,
            "WASM requires Compiler-owned executable functions; call-edge replay and empty entry synthesis are not valid inputs".to_string(),
        ));
    }
    // 导出面判据：声明的 `[export]` 名 ∪ 程序入口，可达闭包不算。
    let mut surface: Vec<WasmCallableKey> = module_input.export_names.keys().copied().collect();
    if let Some(entry) = module_input.entry_operation {
        if !surface.contains(&entry) {
            surface.push(entry);
        }
    }
    for operation in surface {
        if module_input.function(&operation).is_none() {
            return Err(unsound(
                AdapterStage::Function,
                None,
                None,
                format!("WASM callable `{operation}` has no Compiler-owned executable body"),
            ));
        }
    }
    validate_package_export_surface(
        &module_input.export_names,
        &module_input.export_annotations,
        module_input.entry_operation,
        &entry_export_name,
        effective_package_kind,
    )
    .map_err(|reason| unsound(AdapterStage::Function, None, None, reason))?;
    // 存储类选择的唯一闸门：编码前拒绝无身份名义类型残差。形态判断发生在
    // `storage_for_type` / GC 字段字节之前；本门留在 lane 装配点，编码层只消费
    // 已结算的私有事实。
    validate_storage_identities(&module_input)
        .map_err(|reason| unsound(AdapterStage::Representation, None, None, reason))?;

    // WASI command world 由冻结的产物用途决定：只有进程镜像（`Application` /
    // `TestBundle`）打包为 command component，`Library` 打包为库组件。
    // 禁止按「是否存在函数实例」猜测 command——库片段同样有函数实例。
    let package_as_wasi_command = host_boundary == HostProjectionBoundary::WasiComponent
        && matches!(artifact_purpose, ArtifactPurpose::Application | ArtifactPurpose::TestBundle);
    // 库导出签名只从已冻结事实派生：载体来自私有 `wasm_export_abi`（与
    // `nyar.library_invoke` 消费的槽位同源），形参名来自 canonical 参数身份。
    // 缺函数体或参数身份不合法都是结构化失败，不写 `None` 冒充未投影。
    // 只有库用途消费导出面；应用/测试镜像不得因库专属的载体约束而失败。
    let mut library_wasm_exports = Vec::new();
    if matches!(artifact_purpose, ArtifactPurpose::Library) {
        for (instance, source_name) in &module_input.export_names {
            let public_name = module_input.export_public_name(instance).expect("导出实例必有公开名").clone();
            let function = module_input
                .function(instance)
                .ok_or_else(|| unsound(AdapterStage::Function, None, Some(public_name.clone()), format!("Wasm 库导出 `{public_name}` 缺少 Compiler 函数体，无法冻结导出签名")))?;
            let abi = wasm_export_abi(&module_input, instance)
                .map_err(|reason| unsound(AdapterStage::Function, None, Some(public_name.clone()), reason))?;
            let parameter_names = canonical_parameter_identities(&public_name, function)
                .map_err(|reason| unsound(AdapterStage::Function, None, Some(public_name.clone()), reason))?
                .into_iter()
                .map(|(_, name)| name)
                .collect::<Vec<_>>();
            library_wasm_exports.push(WasmLibraryExport {
                source_name: source_name.clone(),
                public_name,
                parameters: abi.params.iter().map(|carrier| carrier.as_str().to_string()).collect(),
                parameter_names,
                returns: abi.returns.as_str().to_string(),
            });
        }
    }
    // 测试包：每个 `[test]` 的启动身份只来自片段冻结的公开名。这里只把
    // 「逻辑测试名 → wasm 导出名」原样冻结进目标输入，JS 启动壳按 argv 查表，
    // 不在壳里重新推导或回退到第一个导出。
    let test_entry_exports = if artifact_purpose == ArtifactPurpose::TestBundle && host_boundary == HostProjectionBoundary::WasmJsGlue {
        module_input
            .export_names
            .keys()
            .map(|instance| {
                let public_name = module_input
                    .export_public_name(instance)
                    .ok_or_else(|| format!("测试入口实例 `{instance}` 缺少冻结公开名"))?;
                Ok(WasmTestEntryExport { logical_name: public_name.clone(), export_name: public_name.clone() })
            })
            .collect::<Result<Vec<_>, String>>()
            .map_err(|reason| unsound(AdapterStage::Function, None, None, reason))?
    }
    else {
        Vec::new()
    };
    Ok(WasmProgramInput::new(
        module_input,
        entry_export_name.clone(),
        host_boundary,
        wasi_preview,
        effective_package_kind,
        package_as_wasi_command,
        library_wasm_exports,
        test_entry_exports,
        // 二进制 JS-glue 包的启动壳必须调用冻结的入口导出名；库包暴露导出面，
        // WASI 由 wasmtime 直接启动 `_start`，都不经启动壳猜入口。
        (effective_package_kind == WasmPackageKind::Binary && host_boundary == HostProjectionBoundary::WasmJsGlue).then(|| entry_export_name),
    ))
}

/// 包型导出面判据（纯函数，供单测直接覆盖）：
/// - `Library`：至少一个 `[export]` 符号；
/// - `Binary`：必须有已解析入口；其它导出的公开名不得占用宿主入口符号；
///   导出公开名必须唯一（宿主装载身份，重复会产出重复导出名的非法模块）。
fn validate_package_export_surface(
    export_names: &BTreeMap<WasmCallableKey, String>,
    export_annotations: &BTreeMap<WasmCallableKey, String>,
    entry: Option<WasmCallableKey>,
    boundary_entry_name: &str,
    kind: WasmPackageKind,
) -> Result<(), String> {
    let public_name = |instance: &WasmCallableKey| export_annotations.get(instance).or_else(|| export_names.get(instance));
    match kind {
        WasmPackageKind::Library => {
            if export_names.is_empty() {
                return Err("library wasm package requires at least one `[export]` symbol".to_string());
            }
        }
        WasmPackageKind::Binary => {
            let entry = entry.ok_or_else(|| "WASM binary package requires a Compiler-resolved entry".to_string())?;
            // 宿主入口名是 lane 保留符号：其它导出的公开名不得占用，否则入口导出与
            // `[export]` 导出会写进同一个名字，产出重复导出名的非法模块。
            if let Some(instance) = export_names.keys().copied().find(|instance| {
                *instance != entry && public_name(instance).is_some_and(|name| name.as_str() == boundary_entry_name)
            }) {
                return Err(format!("WASM binary package: `[export]` 实例 `{instance}` 的公开名与宿主入口符号 `{boundary_entry_name}` 冲突"));
            }
            // 导出公开名是宿主装载身份，必须唯一；重复会让模块写出重复导出名。
            // 库模式在构建 `nyar.library_invoke` 时以同一规则拒绝重复，这里覆盖二进制包。
            let mut public_names = BTreeSet::new();
            for instance in export_names.keys() {
                let name = public_name(instance).ok_or_else(|| format!("WASM binary package: 导出实例 `{instance}` 缺少公开名"))?;
                if !public_names.insert(name.clone()) {
                    return Err(format!("WASM binary package: 导出公开名 `{name}` 重复"));
                }
            }
        }
    }
    Ok(())
}

/// 文本 ABI 站点的准入守卫（私有镜像）：编码身份只由声明的类型事实与
/// `host_boundary` 决定（`declared_text_projection`），不看函数名或包名。
/// 只有直接位于 ABI 位置的 `utf16` 才有冻结契约；嵌套在聚合中的 `utf16` 与
/// `utf16` 字面量没有声明位置，仍按原错误拒绝。
fn validate_text_encoding_projection(input: &WasmModuleInput, host_boundary: HostProjectionBoundary) -> Result<(), String> {
    let text_abi_boundary = host_text_abi_boundary(host_boundary);
    for function in input.functions.values() {
        let mut sites: Vec<(String, &WasmTypeFact)> = vec![("return type".to_string(), &function.return_type)];
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
                if let WasmOperation::LoadConstant { constant: WasmConstant::Utf16(_), .. } = &instruction.kind {
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

fn text_abi_site_denied(ty: &WasmTypeFact, boundary: Option<PhysicalTextBoundary>) -> bool {
    if !type_contains_utf16(ty) {
        return false;
    }
    match ty {
        // 只有直接位于 ABI 位置的 `utf16` 才查冻结事实；聚合内的 `utf16`
        // 没有独立的 ABI 位置，保持原有拒绝。
        WasmTypeFact::Utf16 => match boundary {
            Some(boundary) => declared_text_projection(boundary, ty).is_none(),
            None => true,
        },
        _ => true,
    }
}

fn type_contains_utf16(ty: &WasmTypeFact) -> bool {
    match ty {
        WasmTypeFact::Utf16 => true,
        WasmTypeFact::Apply(base, arguments) => type_contains_utf16(base) || arguments.iter().any(type_contains_utf16),
        WasmTypeFact::Function(function) => type_contains_utf16(&function.return_type) || function.params.iter().any(type_contains_utf16),
        WasmTypeFact::Tuple(elements) | WasmTypeFact::Union(elements) => elements.iter().any(type_contains_utf16),
        WasmTypeFact::Array(element) | WasmTypeFact::FixedArray { element, .. } => type_contains_utf16(element),
        WasmTypeFact::TraitObject(object) => object.type_arguments.iter().any(type_contains_utf16),
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

/// 存储类身份门（私有镜像）：无 declaration / `TypeId` 的名义类型残差在编码前
/// 结构化拒绝。裸 `Named` 短名或 `Nominal` 携带未注册布局时，存储类只能靠名称
/// 回查或默认值猜实现，必须在生成任何字节之前失败。
fn validate_storage_identities(input: &WasmModuleInput) -> Result<(), String> {
    let registered_layouts: BTreeSet<u32> = input.layouts.iter().map(|layout| layout.id).collect();
    for function in input.functions.values() {
        let mut sites: Vec<(String, &WasmTypeFact)> = vec![("return type".to_string(), &function.return_type)];
        sites.extend(function.param_types.iter().enumerate().map(|(index, ty)| (format!("parameter {index}"), ty)));
        sites.extend(function.value_types.iter().map(|(value, ty)| (format!("SSA value {}", value.0), ty)));
        for block in &function.blocks {
            for (index, instruction) in block.instructions.iter().enumerate() {
                let location = format!("block {} instruction {index}", block.id.0);
                match &instruction.kind {
                    WasmOperation::LoadConstant { ty: Some(ty), .. } => sites.push((format!("{location} LoadConstant"), ty)),
                    WasmOperation::StoreVar { ty: Some(ty), .. } => sites.push((format!("{location} StoreVar"), ty)),
                    WasmOperation::ArrayNew { array_type, .. } => sites.push((format!("{location} ArrayNew"), array_type)),
                    WasmOperation::ArrayFromElements { array_type, .. } => sites.push((format!("{location} ArrayFromElements"), array_type)),
                    _ => {}
                }
            }
        }
        for (site, ty) in sites {
            if let Some(ty) = named_residual(ty, &registered_layouts) {
                return Err(storage_error(&function.symbol, &site, &ty));
            }
        }
    }
    for layout in &input.layouts {
        for field in &layout.fields {
            if let Some(ty) = named_residual(&field.ty, &registered_layouts) {
                return Err(storage_error("", &format!("aggregate layout `{}::{}` field `{}`", layout.namespace, layout.name, field.name), &ty));
            }
        }
    }
    for sum in input.sum_layouts_by_type.values() {
        for variant in &sum.variants {
            if let Some(ty) = variant.payload_type.as_ref().and_then(|payload| named_residual(payload, &registered_layouts)) {
                return Err(storage_error("", &format!("unite layout `{}` variant `{}` payload", sum.name, variant.name), &ty));
            }
        }
    }
    Ok(())
}

fn storage_error(function: &str, site: &str, ty: &WasmTypeFact) -> String {
    let owner = if function.is_empty() { site.to_string() } else { format!("{function} at {site}") };
    format!("WASM pre-emission verifier: {owner} 携带无身份名义类型 `{ty}`；declaration / `TypeId` 缺失时不得按短名解析存储类")
}

/// 递归查找无法按身份解析存储类的名义形状：
/// 裸 `Named` 短名残差，或 `Nominal` 携带的 `layout` 未在结算布局中注册。
fn named_residual(ty: &WasmTypeFact, registered_layouts: &BTreeSet<u32>) -> Option<WasmTypeFact> {
    match ty {
        WasmTypeFact::Named(_) => Some(ty.clone()),
        WasmTypeFact::Nominal { layout, .. } => {
            if registered_layouts.contains(layout) {
                None
            }
            else {
                Some(ty.clone())
            }
        }
        WasmTypeFact::Apply(_base, arguments) => arguments.iter().find_map(|argument| named_residual(argument, registered_layouts)),
        WasmTypeFact::Function(function) => named_residual(&function.return_type, registered_layouts)
            .or_else(|| function.params.iter().find_map(|parameter| named_residual(parameter, registered_layouts))),
        WasmTypeFact::Tuple(elements) | WasmTypeFact::Union(elements) => elements.iter().find_map(|element| named_residual(element, registered_layouts)),
        WasmTypeFact::Array(element) | WasmTypeFact::FixedArray { element, .. } | WasmTypeFact::Nullable(element) => {
            named_residual(element, registered_layouts)
        }
        WasmTypeFact::TraitObject(object) => object.type_arguments.iter().find_map(|argument| named_residual(argument, registered_layouts)),
        _ => None,
    }
}

#[cfg(test)]
mod wasm_lane_tests {
    use super::*;
    use compiler_emitter::emitter_backend_wasm::{
        WasmBlock, WasmBlockKey, WasmFunctionInput, WasmInstruction, WasmInstructionKey, WasmSingletonFact, WasmTerminator,
    };

    fn testing_export(export_names: &mut BTreeMap<WasmCallableKey, String>, index: u32, name: &str) {
        export_names.insert(WasmCallableKey(index), name.to_string());
    }

    #[test]
    fn wasm_binary_entry_uses_boundary_symbol_and_rejects_export_collision() {
        // 宿主入口符号 `main` 是 lane 保留名：其它 `[export]` 的公开名不得占用。
        let mut export_names = BTreeMap::new();
        testing_export(&mut export_names, 1, "main");
        let error = validate_package_export_surface(&export_names, &BTreeMap::new(), Some(WasmCallableKey(2)), "main", WasmPackageKind::Binary)
            .expect_err("入口与同名导出必须拒绝");
        assert!(error.contains("main") && error.contains("冲突"), "{error}");
    }

    #[test]
    fn wasm_binary_rejects_duplicate_export_public_names() {
        // 导出公开名是宿主装载身份，重复会产出重复导出名的非法模块。
        let mut export_names = BTreeMap::new();
        testing_export(&mut export_names, 1, "a");
        testing_export(&mut export_names, 2, "b");
        let mut export_annotations = BTreeMap::new();
        export_annotations.insert(WasmCallableKey(1), "dup".to_string());
        export_annotations.insert(WasmCallableKey(2), "dup".to_string());
        let error = validate_package_export_surface(&export_names, &export_annotations, Some(WasmCallableKey(3)), "main", WasmPackageKind::Binary)
            .expect_err("重复公开名必须拒绝");
        assert!(error.contains("重复") && error.contains("dup"), "{error}");
    }

    #[test]
    fn wasm_binary_allows_distinct_public_names() {
        let mut export_names = BTreeMap::new();
        testing_export(&mut export_names, 1, "run");
        assert!(
            validate_package_export_surface(&export_names, &BTreeMap::new(), Some(WasmCallableKey(2)), "main", WasmPackageKind::Binary).is_ok(),
            "不冲突的导出面必须通过"
        );
    }

    #[test]
    fn wasm_binary_requires_resolved_entry() {
        let mut export_names = BTreeMap::new();
        testing_export(&mut export_names, 1, "run");
        let error = validate_package_export_surface(&export_names, &BTreeMap::new(), None, "main", WasmPackageKind::Binary)
            .expect_err("二进制包缺入口必须拒绝");
        assert!(error.contains("entry"), "{error}");
    }

    #[test]
    fn wasm_library_requires_export_surface() {
        let error = validate_package_export_surface(&BTreeMap::new(), &BTreeMap::new(), None, "main", WasmPackageKind::Library)
            .expect_err("库包缺导出面必须拒绝");
        assert!(error.contains("[export]"), "{error}");
    }

    fn testing_function(symbol: &str, return_type: WasmTypeFact, blocks: Vec<WasmBlock>) -> WasmFunctionInput {
        WasmFunctionInput {
            symbol: symbol.to_string(),
            return_type,
            param_types: Vec::new(),
            value_types: BTreeMap::new(),
            value_layouts: BTreeMap::new(),
            return_layout: None,
            entry: WasmBlockKey(0),
            values: Vec::new(),
            blocks,
            parameter_identities: Vec::new(),
        }
    }

    fn testing_module_input(function: WasmFunctionInput) -> WasmModuleInput {
        let key = WasmCallableKey(0);
        let mut functions = BTreeMap::new();
        functions.insert(key, function);
        WasmModuleInput {
            functions,
            instances: vec![key],
            entry_operation: Some(key),
            export_names: BTreeMap::new(),
            export_annotations: BTreeMap::new(),
            exported_operations: Vec::new(),
            abi_labels: BTreeMap::new(),
            module_name: String::new(),
            fragment_id: String::new(),
            required_capabilities: Vec::new(),
            theory_rules: 0,
            theory_equations: 0,
            imports: BTreeMap::new(),
            external_import_links: BTreeMap::new(),
            layouts: Vec::new(),
            layout_by_nominal_id: BTreeMap::new(),
            layout_by_field_id: BTreeMap::new(),
            layout_by_type: BTreeMap::new(),
            sum_layouts_by_type: BTreeMap::new(),
            sum_reps: BTreeMap::new(),
            program_text_representation: None,
            singleton_facts: Vec::<WasmSingletonFact>::new(),
            host_sequence_projections: BTreeSet::new(),
        }
    }

    #[test]
    fn text_gate_denies_utf16_abi_site_without_contract() {
        // WasiComponent 没有 utf16 的文本 ABI 事实：直接位于返回位的 utf16 必须拒绝。
        let input = testing_module_input(testing_function("probe", WasmTypeFact::Utf16, Vec::new()));
        let error = validate_text_encoding_projection(&input, HostProjectionBoundary::WasiComponent)
            .expect_err("utf16 返回位无冻结契约必须拒绝");
        assert!(error.contains("requires an explicit UTF-16 ABI contract"), "{error}");
    }

    #[test]
    fn text_gate_denies_utf16_literal_without_contract() {
        let block = WasmBlock {
            id: WasmBlockKey(0),
            label: "block_0".to_string(),
            parameters: Vec::new(),
            instructions: vec![WasmInstruction {
                id: WasmInstructionKey(0),
                results: Vec::new(),
                kind: WasmOperation::LoadConstant { constant: WasmConstant::Utf16("x".to_string()), ty: Some(WasmTypeFact::Utf16) },
            }],
            terminator: WasmTerminator::Unreachable,
        };
        let input = testing_module_input(testing_function("probe", WasmTypeFact::Unit, vec![block]));
        let error = validate_text_encoding_projection(&input, HostProjectionBoundary::WasiComponent)
            .expect_err("utf16 字面量无冻结契约必须拒绝");
        assert!(error.contains("contains a UTF-16 literal"), "{error}");
    }

    #[test]
    fn storage_gate_denies_bare_named_residual() {
        // 裸 `Named` 短名没有 declaration / `TypeId`：存储类不得按短名解析。
        let input = testing_module_input(testing_function("probe", WasmTypeFact::Named(compiler_types::Identifier::new("Foo")), Vec::new()));
        let error = validate_storage_identities(&input).expect_err("无身份名义残差必须拒绝");
        assert!(error.contains("携带无身份名义类型") && error.contains("Foo"), "{error}");
    }

    #[test]
    fn storage_gate_accepts_identity_types() {
        let input = testing_module_input(testing_function("probe", WasmTypeFact::Integer64 { signed: true }, Vec::new()));
        validate_storage_identities(&input).expect("已闭合类型必须通过");
    }

}
