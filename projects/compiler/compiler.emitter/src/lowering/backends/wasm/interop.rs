//! Wasm lane 的公共导入链接 → 宿主目标分类器。
//!
//! 编码层（`mir/imports`）只消费 `input.rs` 结算后的私有链接事实；本模块保留
//! 面向跨目标公共 `ExternalImportLink` 的同名分类器，供 `shared::interop` 的
//! 候选唯一性裁决测试消费。分类逻辑与 `mir/imports.rs` 私有版逐字一致。
#![allow(dead_code)]

use compiler_target::ExternalImportLink;

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
pub(crate) fn wasm_host_import_target(link: &ExternalImportLink) -> Option<WasmHostImportTarget<'_>> {
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
pub(crate) fn js_glue_host_import_target(link: &ExternalImportLink) -> Option<WasmHostImportTarget<'_>> {
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

pub(crate) fn wasm_or_js_glue_host_import_target(link: &ExternalImportLink) -> Option<WasmHostImportTarget<'_>> {
    wasm_host_import_target(link).or_else(|| js_glue_host_import_target(link))
}

/// 源码 `[wasm_operator("i32.add")]` 声明的 wasm 目标 operator 助记符。
///
/// 只接受 platform tag `wasm-operator` 且 locator 恰好一段；其余形态返回 `None`，
/// 由调用方按「缺少可编码目标绑定」结构化失败。
pub(crate) fn wasm_operator_target(link: &ExternalImportLink) -> Option<&str> {
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
pub(crate) fn wasi_host_import_target(link: &ExternalImportLink) -> Option<WasiHostImportTarget<'_>> {
    if !link.matches_boundary("host") || !link.matches_platform_tag("wasi") {
        return None;
    }

    match link.locator_segments() {
        [module, function] => Some(WasiHostImportTarget { module, function }),
        _ => None,
    }
}
