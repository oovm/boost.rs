//! Host import collection / validation / index-building for Node + WASI.
//!
//! 本模块只消费 `input.rs` 结算出的私有链接事实（[`WasmImportLinkFact`]）；
//! 面向公共 `ExternalImportLink` 的同名分类器在 `wasm/interop.rs`。

use std::collections::{BTreeMap, BTreeSet};

use crate::emitter_backend_wasi::WasiPreview;

use super::super::sections::wasm_function_type;
use super::*;

/// Wasm 宿主导入的模块/字段对。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WasmHostImportTarget<'a> {
    pub module: &'a str,
    pub field: &'a str,
}

/// WASI 宿主导入的模块/函数对。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WasiHostImportTarget<'a> {
    pub module: &'a str,
    pub function: &'a str,
}

/// `[wasm("module", "field")]` 声明的 wasm 宿主导入。
pub(crate) fn wasm_host_import_target(link: &WasmImportLinkFact) -> Option<WasmHostImportTarget<'_>> {
    if !link.matches_boundary("host") || !link.matches_platform_tag("wasm") {
        return None;
    }

    match link.locator_segments() {
        [module, field, ..] => Some(WasmHostImportTarget { module, field }),
        _ => None,
    }
}

/// `[js_builtin("wx.request")]` → browser/node wasm `env` 导入字段。
///
/// WasmJsGlue 轨在 MIR 阶段仍通过 wasm `call` 命中宿主导入；`.mjs` 启动壳按字段名提供实现。
pub(crate) fn js_glue_host_import_target(link: &WasmImportLinkFact) -> Option<WasmHostImportTarget<'_>> {
    if !link.matches_boundary("host") {
        return None;
    }
    if !link.matches_platform_tag("js_builtin") {
        return None;
    }

    match link.locator_segments() {
        [field] => Some(WasmHostImportTarget { module: "env", field }),
        [module, field, ..] => Some(WasmHostImportTarget { module, field }),
        _ => None,
    }
}

pub(crate) fn wasm_or_js_glue_host_import_target(link: &WasmImportLinkFact) -> Option<WasmHostImportTarget<'_>> {
    wasm_host_import_target(link).or_else(|| js_glue_host_import_target(link))
}

/// 源码 `[wasm_operator("i32.add")]` 声明的 wasm 目标 operator 助记符。
///
/// 只接受 platform tag `wasm-operator` 且 locator 恰好一段；其余形态返回 `None`，
/// 由调用方按「缺少可编码目标绑定」结构化失败。
pub(crate) fn wasm_operator_target(link: &WasmImportLinkFact) -> Option<&str> {
    if !link.matches_boundary("host") || !link.matches_platform_tag("wasm-operator") {
        return None;
    }

    match link.locator_segments() {
        [mnemonic] => Some(mnemonic.as_str()),
        _ => None,
    }
}

/// WASI 宿主合同只由显式 `wasi` 平台标签绑定：不凭 locator 的 `wasi:` 前缀
/// 猜测，否则 JS 目标里恰好叫 `wasi:*` 的模块会被误当成 WASI 宿主导入。
pub(crate) fn wasi_host_import_target(link: &WasmImportLinkFact) -> Option<WasiHostImportTarget<'_>> {
    if !link.matches_boundary("host") || !link.matches_platform_tag("wasi") {
        return None;
    }

    match link.locator_segments() {
        [module, function] => Some(WasiHostImportTarget { module, function }),
        _ => None,
    }
}

/// 候选唯一性裁决（wasm 私有版；错误文案与共享 `select_unique_target` 逐字一致）。
///
/// 同一 import 里若有多条候选同时被同一个裁决函数命中，声明本身有歧义：
/// 必须结构化失败，禁止按声明顺序 first-wins。
fn select_unique_target<'a, T>(
    links: &'a [WasmImportLinkFact],
    target: impl Fn(&'a WasmImportLinkFact) -> Option<T>,
) -> crate::Result<Option<(&'a WasmImportLinkFact, T)>> {
    let matched = links.iter().filter_map(|link| target(link).map(|value| (link, value))).collect::<Vec<_>>();
    if matched.len() <= 1 {
        return Ok(matched.into_iter().next());
    }
    let candidates = matched
        .iter()
        .map(|(link, _)| {
            let tag = link.platform_tag.as_ref().map(|tag| tag.as_str()).unwrap_or("<none>");
            format!("tag=`{tag}` locator=[{}]", link.locator_segments().join(", "))
        })
        .collect::<Vec<_>>()
        .join("; ");
    Err(format!(
        "外部导入目标绑定歧义：{} 条候选同时命中同一 target 裁决，必须唯一（{candidates}）",
        matched.len()
    ))
}

pub(super) fn collect_wasm_host_imports(input: &WasmModuleInput, synthesize_const_utf8: bool) -> crate::Result<Vec<(String, String)>> {
    let mut imports = Vec::new();
    let mut seen = BTreeSet::new();
    // 计划声明的导入是权威集合：`external_import_links` 与 `imports` 在完整流水线里
    // 应当同源，但测试夹具可能只登记其一；两边都收集，按 `(module, field)` 去重。
    let declared_links = input.external_import_links.values().chain(input.imports.values().map(|import| &import.links));
    for links in declared_links {
        // `[wasm]` 与 `[js_builtin]` 是同一宿主表面的两种合法声明，共用同一裁决函数；
        // 一个 import 同时命中两条即歧义，由选择函数失败闭合。
        let Some(target) = select_unique_target(links, wasm_or_js_glue_host_import_target)?
        else {
            continue;
        };
        let key = (target.1.module.to_string(), target.1.field.to_string());
        if seen.insert(key.clone()) {
            imports.push(key);
        }
    }
    if synthesize_const_utf8 {
        let key = ("env".to_string(), "const_utf8".to_string());
        if seen.insert(key.clone()) {
            imports.push(key);
        }
    }
    // JS 启动壳协议的内建能力（不再是语言 std 文本操作——那些归 provider）。
    for field in crate::emitter_backend_wasm_js_glue::SYNTHETIC_NODE_IMPORT_FIELDS {
        let key = ("env".to_string(), (*field).to_string());
        if seen.insert(key.clone()) {
            imports.push(key);
        }
    }
    // 语言 × 目标适配器显式登记的宿主导入 provider：语言特有宿主行为只在
    // provider 集合里出现，wasm 降层不再内建任何语言 std 字段。
    for provider in &input.host_import_providers {
        let key = (provider.module.clone(), provider.field.clone());
        if seen.insert(key.clone()) {
            imports.push(key);
        }
    }
    imports.sort();
    Ok(imports)
}

pub(super) fn collect_wasi_host_imports(input: &WasmModuleInput, preview: WasiPreview) -> crate::Result<Vec<(String, String)>> {
    let mut imports = Vec::new();
    let mut seen = BTreeSet::new();
    for links in input.external_import_links.values() {
        let Some(target) = select_unique_target(links, wasi_host_import_target)?
        else {
            continue;
        };
        let Some((module, field)) = crate::emitter_backend_wasi::wasi_adapt_import_for_preview(target.1.module, target.1.function, preview)
        else {
            continue;
        };
        let key = (crate::emitter_backend_wasi::wasi_versioned_import_module_for(&module, preview), field);
        if seen.insert(key.clone()) {
            imports.push(key);
        }
    }
    for links in input.external_import_links.values() {
        let Some(target) = select_unique_target(links, wasm_host_import_target)?
        else {
            continue;
        };
        let key = (target.1.module.to_string(), target.1.field.to_string());
        if seen.insert(key.clone()) {
            imports.push(key);
        }
    }
    imports.sort();
    crate::emitter_backend_wasi::expand_wasi_cli_stream_imports(&mut imports);
    Ok(imports)
}

pub(super) fn collect_mir_string_literals(input: &WasmModuleInput, operations: &[WasmCallableKey]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut literals = Vec::new();
    for operation in operations {
        let Some(function) = input.function(operation)
        else {
            continue;
        };
        for block in &function.blocks {
            for instruction in &block.instructions {
                if let WasmOperation::LoadConstant { constant: WasmConstant::Utf8(text), .. } = &instruction.kind {
                    if seen.insert(text.clone()) {
                        literals.push(text.clone());
                    }
                }
            }
        }
    }
    literals
}

pub(super) fn const_utf8_import_index(imports: &[(String, String)]) -> Option<u32> {
    imports.iter().position(|(module, field)| module == "env" && field == "const_utf8").map(|index| index as u32)
}

pub(super) fn validate_string_literal_payloads(
    input: &WasmModuleInput,
    operations: &[WasmCallableKey],
    string_literal_index: &BTreeMap<String, u32>,
    string_literal_offset: &BTreeMap<String, u32>,
    const_utf8_import: Option<u32>,
    wasi_mode: bool,
) -> crate::Result<()> {
    for operation in operations {
        let Some(function) = input.function(operation)
        else {
            continue;
        };
        for block in &function.blocks {
            for (index, instruction) in block.instructions.iter().enumerate() {
                let WasmOperation::LoadConstant { constant: WasmConstant::Utf8(text), .. } = &instruction.kind
                else {
                    continue;
                };
                let registered = if wasi_mode {
                    string_literal_offset.contains_key(text)
                }
                else {
                    const_utf8_import.is_some() && string_literal_index.contains_key(text)
                };
                if !registered {
                    return Err(format!(
                        "WASM pre-emission verifier: `{}` block {} instruction {index} 的 utf8 字面量缺少宿主载体（wasi={wasi_mode}, const_utf8={const_utf8_import:?}）；不得回落 `i32 0` 句柄",
                        function.symbol, block.id.0
                    ));
                }
            }
        }
    }
    Ok(())
}

pub(super) fn build_string_literal_index(literals: &[String]) -> BTreeMap<String, u32> {
    literals.iter().enumerate().map(|(index, text)| (text.clone(), index as u32)).collect()
}

/// `env` 合成宿主导入的物理签名。
///
/// 签名事实由 JS glue 协议所有者（[`crate::emitter_backend_wasm_js_glue`]）声明；
/// 本层只做 wasm 指令类型编码，不自行维护字段名匹配表。
pub(super) fn wasm_import_type_for_field(field: &str) -> Option<Vec<u8>> {
    crate::emitter_backend_wasm_js_glue::glue_import_signature(field).map(|(params, results)| wasm_function_type(params, results))
}

/// WASI core 导入的物理签名，事实由 WASI 协议所有者（[`crate::emitter_backend_wasi`]）声明。
pub(super) fn wasi_import_type_for_field(module: &str, field: &str) -> Option<Vec<u8>> {
    crate::emitter_backend_wasi::wasi_import_signature(module, field).map(|(params, results)| wasm_function_type(params, results))
}

/// `(module, field)` → 已在结算输入里绑定的导入签名（`[js]` / `[wasm]` 等）。
pub(super) fn host_import_signature_index(input: &WasmModuleInput) -> crate::Result<BTreeMap<(String, String), WasmImportFact>> {
    let mut index = BTreeMap::new();
    for import in input.imports.values() {
        let Some((_, target)) = select_unique_target(&import.links, wasm_or_js_glue_host_import_target)?
        else {
            continue;
        };
        let key = (target.module.to_string(), target.field.to_string());
        index.entry(key).or_insert_with(|| import.clone());
    }
    Ok(index)
}

pub(super) fn wasm_import_type_from_backend_import(
    input: &WasmModuleInput,
    import: &WasmImportFact,
    wasi_mode: bool,
    js_glue_utf8_as_anyref: bool,
) -> crate::Result<Vec<u8>> {
    // `WasiComponent` 的宿主导入没有冻结的 `utf16` 载体，仍走严格表；
    // `WasmJsGlue` 边界才按 `declared_text_projection` 把 `utf16` 降为 i32 宿主句柄。
    // 导入签名不在 `validate_text_encoding_projection` 的覆盖范围内，所以这里必须
    // 自己区分边界，不能依赖 verifier 兜底。
    let abi_value_type = |ty: &WasmTypeFact| {
        if wasi_mode {
            wasm_param_value_type(input, ty, js_glue_utf8_as_anyref)
        }
        else {
            wasm_js_glue_abi_value_type(input, ty, js_glue_utf8_as_anyref)
        }
    };
    let params = import.parameter_types.iter().map(abi_value_type).collect::<crate::Result<Vec<_>>>()?;
    let results = match &import.return_type {
        WasmTypeFact::Unit | WasmTypeFact::Bottom => Vec::new(),
        ty => vec![abi_value_type(ty)?],
    };
    Ok(wasm_function_type(&params, &results))
}

pub(super) fn wasm_import_type_bytes_for_host(
    input: &WasmModuleInput,
    wasi_mode: bool,
    module: &str,
    field: &str,
    js_glue_utf8_as_anyref: bool,
    signature_index: &BTreeMap<(String, String), WasmImportFact>,
) -> crate::Result<Vec<u8>> {
    // 单一事实源：结算输入里已冻结的 `WasmImportFact` 声明优先。
    // 字段名表只是历史快照，不得越过 plan 改写已声明的导入签名。
    let key = (module.to_string(), field.to_string());
    if let Some(import) = signature_index.get(&key) {
        return wasm_import_type_from_backend_import(input, import, wasi_mode, js_glue_utf8_as_anyref);
    }
    // 适配器提交的 provider 物理签名：语言 std 宿主行为的签名事实由 provider
    // 声明，编码层只做载体到 valtype 的机械映射。
    if !wasi_mode {
        if let Some(bytes) = provider_import_type_bytes(input, module, field) {
            return Ok(bytes);
        }
    }
    // fallback 只服务 plan 不存在的合成宿主导入（`env::const_utf8`、JS 启动壳
    // 协议内建字段、WASI 预演化的 stream 内建等）；它不得覆盖 plan 已声明的事实。
    let fallback = if wasi_mode { wasi_import_type_for_field(module, field) } else { wasm_import_type_for_field(field) };
    fallback.ok_or_else(|| format!("host import `{module}::{field}` has no frozen ABI signature"))
}

/// 适配器提交的宿主导入 provider 的物理签名（`env` 文本 std 等语言行为）。
fn provider_import_type_bytes(input: &WasmModuleInput, module: &str, field: &str) -> Option<Vec<u8>> {
    let provider = input.host_import_providers.iter().find(|item| item.module == module && item.field == field)?;
    let params = provider.params.iter().copied().filter_map(super::wasm_host_provider_carrier_valtype).collect::<Vec<_>>();
    let results = provider.results.iter().copied().filter_map(super::wasm_host_provider_carrier_valtype).collect::<Vec<_>>();
    Some(wasm_function_type(&params, &results))
}

pub(super) fn wasm_import_type_bytes(wasi_mode: bool, module: &str, field: &str) -> crate::Result<Vec<u8>> {
    let resolved = if wasi_mode { wasi_import_type_for_field(module, field) } else { wasm_import_type_for_field(field) };
    resolved.ok_or_else(|| format!("host import `{module}::{field}` has no frozen ABI signature"))
}

pub(super) fn wasi_core_import_name<'a>(module: &'a str, field: &'a str, preview: WasiPreview) -> (&'a str, &'a str) {
    if preview == WasiPreview::Preview2 {
        let bare = module.split('@').next().unwrap_or(module);
        let canonical = match bare {
            "wasi:clocks/monotonic-clock" => Some("cm32p2|wasi:clocks/monotonic-clock@0.2"),
            "wasi:io/streams" => Some("cm32p2|wasi:io/streams@0.2"),
            _ => None,
        };
        if let Some(canonical) = canonical {
            return (canonical, field);
        }
    }
    (module, field)
}

pub(super) fn build_callee_import_index(input: &WasmModuleInput, imports: &[(String, String)]) -> crate::Result<BTreeMap<WasmCallableKey, u32>> {
    let import_index_by_key: BTreeMap<(String, String), u32> =
        imports.iter().enumerate().map(|(index, (module, field))| ((module.clone(), field.clone()), index as u32)).collect();
    let mut callee_import_index = BTreeMap::new();
    for (callee, import) in input.imports.iter() {
        let Some((_, target)) = select_unique_target(&import.links, wasm_or_js_glue_host_import_target)?
        else {
            continue;
        };
        let Some(&index) = import_index_by_key.get(&(target.module.to_string(), target.field.to_string()))
        else {
            continue;
        };
        callee_import_index.insert(*callee, index);
    }
    Ok(callee_import_index)
}

pub(super) fn build_callee_wasm_operator_index(input: &WasmModuleInput) -> crate::Result<BTreeMap<WasmCallableKey, (String, WasmOperatorBinding)>> {
    let mut callee_wasm_operator = BTreeMap::new();
    for (callee, import) in input.imports.iter() {
        let Some((_, mnemonic)) = select_unique_target(&import.links, wasm_operator_target)?
        else {
            continue;
        };
        // operator 导入是按构造没有 ABI 名的外部实例（目标计划在导入分支不写
        // `abi_names`）；这个标签只用于失败信息定位，不得写成空串冒充已冻结符号身份。
        let symbol = input
            .abi_labels
            .get(callee)
            .cloned()
            .unwrap_or_else(|| format!("<wasm-operator import {callee:?}>"));
        let binding = freeze_wasm_operator_binding(*callee, &symbol, mnemonic, &import.parameter_types, &import.return_type)?
            .ok_or_else(|| format!("Wasm operator 绑定 `{mnemonic}` 缺少冻结结果: callee={callee:?}"))?;
        callee_wasm_operator.insert(*callee, (mnemonic.to_string(), binding));
    }
    Ok(callee_wasm_operator)
}

pub(super) fn build_wasi_callee_import_index(
    input: &WasmModuleInput,
    imports: &[(String, String)],
    preview: WasiPreview,
) -> crate::Result<BTreeMap<WasmCallableKey, u32>> {
    let import_index_by_key: BTreeMap<(String, String), u32> =
        imports.iter().enumerate().map(|(index, (module, field))| ((module.clone(), field.clone()), index as u32)).collect();
    let mut callee_import_index = BTreeMap::new();
    for (callee, import) in input.imports.iter() {
        let Some((_, target)) = select_unique_target(&import.links, wasi_host_import_target)?
        else {
            continue;
        };
        let Some((module, field)) = crate::emitter_backend_wasi::wasi_adapt_import_for_preview(target.module, target.function, preview)
        else {
            continue;
        };
        let module = crate::emitter_backend_wasi::wasi_versioned_import_module_for(&module, preview);
        let Some(&index) = import_index_by_key.get(&(module, field))
        else {
            continue;
        };
        callee_import_index.insert(*callee, index);
    }
    for (callee, import) in input.imports.iter() {
        let Some((_, target)) = select_unique_target(&import.links, wasm_host_import_target)?
        else {
            continue;
        };
        let Some(&index) = import_index_by_key.get(&(target.module.to_string(), target.field.to_string()))
        else {
            continue;
        };
        callee_import_index.insert(*callee, index);
    }
    Ok(callee_import_index)
}

#[cfg(test)]
mod host_import_signature_tests {
    use compiler_types::Identifier;

    use super::*;

    #[test]
    fn wasm_import_type_uses_backend_import_when_field_not_frozen() {
        let import_id = WasmCallableKey(1);
        let mut input = WasmModuleInput::testing_empty();
        input.imports.insert(
            import_id,
            WasmImportFact {
                links: vec![WasmImportLinkFact {
                    boundary: Identifier::new("host"),
                    platform_tag: Some(Identifier::new("js_builtin")),
                    locator_segments: vec!["fetch".into()],
                }],
                parameter_types: vec![WasmTypeFact::Integer32 { signed: true }, WasmTypeFact::Integer32 { signed: true }],
                return_type: WasmTypeFact::Integer32 { signed: true },
            },
        );
        let signatures = host_import_signature_index(&input).expect("signature index");
        let type_bytes = wasm_import_type_bytes_for_host(&input, false, "env", "fetch", false, &signatures)
            .expect("js_builtin fetch must derive wasm signature from WasmImportFact");
        let params = super::super::strings::wasm_function_type_param_bytes(&type_bytes);
        assert_eq!(params, vec![VALTYPE_I32, VALTYPE_I32]);
        assert_eq!(super::super::strings::wasm_function_type_result_byte(&type_bytes), Some(VALTYPE_I32));
    }

    #[test]
    fn wasm_import_type_prefers_backend_import_over_field_name_table() {
        let import_id = WasmCallableKey(1);
        let mut input = WasmModuleInput::testing_empty();
        input.imports.insert(
            import_id,
            WasmImportFact {
                links: vec![WasmImportLinkFact {
                    boundary: Identifier::new("host"),
                    platform_tag: Some(Identifier::new("js_builtin")),
                    // `env::host_intern_utf8` 同时存在于字段名表（`[i32, i32] -> [i32]`），
                    // 这里由结算输入声明一个不同的签名，验证 plan 胜出。
                    locator_segments: vec!["host_intern_utf8".into()],
                }],
                parameter_types: vec![WasmTypeFact::Integer32 { signed: true }, WasmTypeFact::Integer32 { signed: true }],
                return_type: WasmTypeFact::Integer64 { signed: true },
            },
        );
        let signatures = host_import_signature_index(&input).expect("signature index");
        assert!(signatures.contains_key(&("env".to_string(), "host_intern_utf8".to_string())), "plan import 必须解析到 env::host_intern_utf8");
        assert_eq!(
            wasm_import_type_for_field("host_intern_utf8"),
            Some(wasm_function_type(&[VALTYPE_I32, VALTYPE_I32], &[VALTYPE_I32])),
            "字段名表快照应保持 `[i32, i32] -> [i32]`"
        );
        let type_bytes =
            wasm_import_type_bytes_for_host(&input, false, "env", "host_intern_utf8", false, &signatures).expect("plan 声明的签名必须胜出");
        assert_eq!(super::super::strings::wasm_function_type_param_bytes(&type_bytes), vec![VALTYPE_I32, VALTYPE_I32]);
        assert_eq!(super::super::strings::wasm_function_type_result_byte(&type_bytes), Some(VALTYPE_I64));
    }
}
