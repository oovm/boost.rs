use compiler_target::ExternalImportLink;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WasmHostImportTarget<'a> {
    pub module: &'a str,
    pub field: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WasiHostImportTarget<'a> {
    pub module: &'a str,
    pub function: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Win32HostImportTarget<'a> {
    pub dll: &'a str,
    pub symbol: &'a str,
}

/// How to repair the evaluation stack after a BCL call whose MSIL return differs from the
/// frontend/host-contract desired type (flatten often copies the contract return onto the FFI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClrHostReturnAdapt {
    /// Stack already matches `desired` (including void with nothing pushed).
    Identity,
    /// Pop the BCL reference return; if `push_true`, push `ldc.i4.1` for a bool contract.
    DiscardRef { push_true: bool },
    /// BCL returns void; push `ldc.i4.1` so a bool contract/local gets a value.
    PushTrue,
}

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

/// 在一份导入声明的全部候选链接里，由本 target 自己的裁决函数选出唯一命中项。
///
/// 语言侧只保留「声明了哪些目标绑定」并保持声明顺序；哪个 target 用哪条由各后端
/// 自己的裁决函数（[`jvm_operator_target`] / [`clr_operator_target`] /
/// [`wasm_operator_target`] / [`ovm_operator_target`] / [`x86_64_operator_target`] /
/// [`wasm_or_js_glue_host_import_target`] / [`wasi_host_import_target`] /
/// [`win32_host_import_target`] / [`linux_gnu_host_import_target`] …）决定。
///
/// 同一 import 里若有多条候选同时被**同一个**裁决函数命中（例如重复写了两次
/// `[jvm_operator]`，或 `[host_contract]` 与目标绑定都归同一 target 消费），
/// 声明本身有歧义：必须结构化失败，禁止按声明顺序 first-wins。
pub(crate) fn select_unique_target<'a, T>(
    links: &'a [ExternalImportLink],
    target: impl Fn(&'a ExternalImportLink) -> Option<T>,
) -> Result<Option<(&'a ExternalImportLink, T)>, String> {
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

/// 源码 `[ovm_operator("i32.add")]` 声明的 OVM 目标 operator 助记符。
///
/// 只接受 platform tag `ovm-operator` 且 locator 恰好一段；其余形态返回 `None`，
/// 由调用方按「缺少可编码目标绑定」结构化失败。
pub(crate) fn ovm_operator_target(link: &ExternalImportLink) -> Option<&str> {
    if !link.matches_boundary("host") || !link.matches_platform_tag("ovm-operator") {
        return None;
    }

    match link.locator_segments() {
        [mnemonic] => Some(mnemonic.as_str()),
        _ => None,
    }
}

/// 源码 `[ovm("symbol")]` 声明的 OVM host provider 符号。
///
/// OVM 的宿主 provider 走 `CallImport`，由 runtime `parse_host_op` 判定；只接受
/// host 边界 + platform tag `ovm` + 恰好一段符号，其余形态返回 `None`。
pub(crate) fn ovm_host_import_target(link: &ExternalImportLink) -> Option<&str> {
    if !link.matches_boundary("host") || !link.matches_platform_tag("ovm") {
        return None;
    }

    match link.locator_segments() {
        [symbol] => Some(symbol.as_str()),
        _ => None,
    }
}

/// 源码 `[jvm_operator("ldiv")]` 声明的 JVM 目标 operator 助记符。
///
/// 只接受 platform tag `jvm-operator` 且 locator 恰好一段；其余形态返回 `None`，
/// 由调用方按「缺少可编码目标绑定」结构化失败。
pub(crate) fn jvm_operator_target(link: &ExternalImportLink) -> Option<&str> {
    if !link.matches_boundary("host") || !link.matches_platform_tag("jvm-operator") {
        return None;
    }

    match link.locator_segments() {
        [mnemonic] => Some(mnemonic.as_str()),
        _ => None,
    }
}

/// 源码 `[clr_operator("i64.div")]` 声明的 CLR 目标 operator 助记符。
///
/// 只接受 platform tag `clr-operator` 且 locator 恰好一段；其余形态返回 `None`，
/// 由调用方按「缺少可编码目标绑定」结构化失败。
pub(crate) fn clr_operator_target(link: &ExternalImportLink) -> Option<&str> {
    if !link.matches_boundary("host") || !link.matches_platform_tag("clr-operator") {
        return None;
    }

    match link.locator_segments() {
        [mnemonic] => Some(mnemonic.as_str()),
        _ => None,
    }
}

/// 源码 `[x86_64_operator("i64.add")]` 声明的 x86-64 目标 operator 助记符。
///
/// 注解按 **ISA** 声明，而不是按「native」：`native` 只是产物形态，`native` /
/// `node` / `python` 最终都会展开成规范目标四元组，而算子编码由四元组的 `arch`
/// 决定。计划层（[`crate::TargetExecutionPlan`]）不持有目标四元组，因此只有显式
/// 声明 ISA 的绑定才能在此冻结编码；声明 `native` 而不声明 ISA 的绑定保持
/// `Unfrozen`，由 emit 结构化失败，禁止按宿主架构猜编码。
///
/// 只接受 platform tag `x86_64-operator` 且 locator 恰好一段；其余形态返回 `None`。
pub(crate) fn x86_64_operator_target(link: &ExternalImportLink) -> Option<&str> {
    if !link.matches_boundary("host") || !link.matches_platform_tag("x86_64-operator") {
        return None;
    }

    match link.locator_segments() {
        [mnemonic] => Some(mnemonic.as_str()),
        _ => None,
    }
}

pub(crate) fn wasi_host_import_target(link: &ExternalImportLink) -> Option<WasiHostImportTarget<'_>> {
    // WASI 宿主合同只由显式 `wasi` 平台标签绑定：不凭 locator 的 `wasi:` 前缀
    // 猜测，否则 JS 目标里恰好叫 `wasi:*` 的模块会被误当成 WASI 宿主导入。
    if !link.matches_boundary("host") || !link.matches_platform_tag("wasi") {
        return None;
    }

    match link.locator_segments() {
        [module, function] => Some(WasiHostImportTarget { module, function }),
        _ => None,
    }
}

pub(crate) fn win32_host_import_target(link: &ExternalImportLink) -> Option<Win32HostImportTarget<'_>> {
    if !link.matches_host_platform("win32") {
        return None;
    }

    match link.locator_segments() {
        [dll, symbol] => Some(Win32HostImportTarget { dll, symbol }),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LinuxGnuHostImportTarget<'a> {
    pub library: &'a str,
    pub symbol: &'a str,
}

pub(crate) fn linux_gnu_host_import_target(link: &ExternalImportLink) -> Option<LinuxGnuHostImportTarget<'_>> {
    if link.matches_host_platform("linux-gnu") || link.matches_host_platform("linux") {
        return match link.locator_segments() {
            [library, symbol] => Some(LinuxGnuHostImportTarget { library, symbol }),
            _ => None,
        };
    }

    // Freestanding Linux print / I/O edges may be declared as `[syscall(N)]`
    // without a platform tag; recognize them as linux-gnu host imports.
    match link.locator_segments() {
        [library, number] if library.as_str() == "syscall" => Some(LinuxGnuHostImportTarget { library, symbol: number }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use compiler_target::Identifier;

    /// 同一份声明携带的四个目标绑定（与 `std_operators.v` 的 `i32` `+` 一致）。
    fn four_target_candidates() -> Vec<ExternalImportLink> {
        vec![
            ExternalImportLink::host(Some(Identifier::new("jvm-operator")), vec!["integer.add".into()]),
            ExternalImportLink::host(Some(Identifier::new("clr-operator")), vec!["i32.add".into()]),
            ExternalImportLink::host(Some(Identifier::new("wasm-operator")), vec!["i32.add".into()]),
            ExternalImportLink::host(Some(Identifier::new("ovm-operator")), vec!["i32.add".into()]),
        ]
    }

    #[test]
    fn same_declaration_serves_every_target_by_its_own_adjudicator() {
        let links = four_target_candidates();

        let (jvm_link, jvm) = select_unique_target(&links, jvm_operator_target).expect("jvm 裁决唯一").expect("jvm 绑定");
        assert_eq!(jvm, "integer.add");
        assert!(std::ptr::eq(jvm_link, &links[0]), "JVM 必须选中声明里的 jvm-operator 候选");

        let (clr_link, clr) = select_unique_target(&links, clr_operator_target).expect("clr 裁决唯一").expect("clr 绑定");
        assert_eq!(clr, "i32.add");
        assert!(std::ptr::eq(clr_link, &links[1]), "CLR 必须选中声明里的 clr-operator 候选");

        let (wasm_link, wasm) = select_unique_target(&links, wasm_operator_target).expect("wasm 裁决唯一").expect("wasm 绑定");
        assert_eq!(wasm, "i32.add");
        assert!(std::ptr::eq(wasm_link, &links[2]), "Wasm 必须选中声明里的 wasm-operator 候选");

        let (ovm_link, ovm) = select_unique_target(&links, ovm_operator_target).expect("ovm 裁决唯一").expect("ovm 绑定");
        assert_eq!(ovm, "i32.add");
        assert!(std::ptr::eq(ovm_link, &links[3]), "OVM 必须选中声明里的 ovm-operator 候选");

        assert!(
            select_unique_target(&links, x86_64_operator_target).expect("x86_64 裁决唯一").is_none(),
            "该声明没有 native 绑定；未命中必须保持 None，不得回退到别的 target"
        );
    }

    #[test]
    fn wasi_binding_requires_explicit_domain_not_locator_prefix() {
        let tagged = ExternalImportLink::host(Some(Identifier::new("wasi")), vec!["wasi:io/streams".into(), "blocking-write".into()]);
        let target = wasi_host_import_target(&tagged).expect("显式 `wasi` 域必须解析成 WASI 宿主导入");
        assert_eq!(target.module, "wasi:io/streams");
        assert_eq!(target.function, "blocking-write");

        // 同一 locator 但域是 JS/Wasm32：不得凭 `wasi:` 前缀猜成 WASI 宿主。
        for domain in [None, Some("wasm32-unknown"), Some("js")] {
            let js = ExternalImportLink::host(domain.map(Identifier::new), vec!["wasi:io/streams".into(), "blocking-write".into()]);
            assert!(
                wasi_host_import_target(&js).is_none(),
                "域 {domain:?} 下的 `wasi:` locator 不得被接受为 WASI 宿主导入"
            );
        }
    }

    #[test]
    fn missing_target_provider_is_none_not_borrowed_from_other_domain() {
        // 声明里只有 `jvm-operator`：其他目标各自的裁决必须是 None，不得把相邻域的
        // 绑定借来当自己的 provider。
        let links = vec![ExternalImportLink::host(Some(Identifier::new("jvm-operator")), vec!["integer.add".into()])];
        for (label, matched) in [
            ("ovm", select_unique_target(&links, ovm_host_import_target).expect("ovm 裁决唯一")),
            ("clr", select_unique_target(&links, clr_operator_target).expect("clr 裁决唯一")),
            ("wasm", select_unique_target(&links, wasm_operator_target).expect("wasm 裁决唯一")),
            ("x86_64", select_unique_target(&links, x86_64_operator_target).expect("x86_64 裁决唯一")),
        ] {
            assert!(matched.is_none(), "{label} 不得借用其他域的目标绑定");
        }
    }

    #[test]
    fn duplicate_binding_for_same_target_fails_closed() {
        let links = vec![
            ExternalImportLink::host(Some(Identifier::new("jvm-operator")), vec!["integer.add".into()]),
            ExternalImportLink::host(Some(Identifier::new("jvm-operator")), vec!["integer.subtract".into()]),
        ];

        let error = select_unique_target(&links, jvm_operator_target).expect_err("重复目标绑定必须歧义失败，禁止 first-wins");
        assert!(error.contains("歧义"), "错误必须是歧义形态: {error}");
        assert!(error.contains("jvm-operator"), "错误必须指出命中的 platform tag: {error}");
        assert!(error.contains("integer.add") && error.contains("integer.subtract"), "错误必须列出全部候选: {error}");
    }
}
