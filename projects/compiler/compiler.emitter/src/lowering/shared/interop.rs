//! 外部导入目标绑定的共享裁决工具。
//!
//! 架构合同：各协议的绑定解码（`[jvm_operator]` / `[clr_operator]` / `[wasm_operator]` /
//! `[ovm_operator]` / `[x86_64_operator]` 与各宿主导入域）归**语言 × 目标适配器**：
//! JVM / CLR / OVM / x86-64 分类器已迁至 `language.valkyrie` 的
//! `compiler::emitter::pipeline::<lane>::operator`；Wasm 分类器仍在
//! `crate::lowering::backends::wasm::interop`（测试门）。本模块只保留可跨 lane
//! 复用的**候选唯一性校验**，不携带任何平台的绑定词汇。

use compiler_target::ExternalImportLink;

/// 在一份导入声明的全部候选链接里，由本 target 自己的裁决函数选出唯一命中项。
///
/// 语言侧只保留「声明了哪些目标绑定」并保持声明顺序；哪个 target 用哪条由各后端
/// 自己的裁决函数决定。
///
/// 同一 import 里若有多条候选同时被**同一个**裁决函数命中（例如重复写了两次
/// `[jvm_operator]`，或 `[host_contract]` 与目标绑定都归同一 target 消费），
/// 声明本身有歧义：必须结构化失败，禁止按声明顺序 first-wins。
pub fn select_unique_target<'a, T>(
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

#[cfg(test)]
mod tests {
    use super::*;
    use compiler_target::Identifier;
    use crate::lowering::backends::wasm::interop::{wasi_host_import_target, wasm_operator_target};

    /// 复刻语言 × 目标适配器分类器形状的本地分类器：`*_operator_target` 已迁至
    /// `language.valkyrie` 的 pipeline，本测试门只验证 `select_unique_target` 的
    /// 裁决语义（唯一命中、歧义失败、跨域不借用），不重复裁决各平台的绑定词汇。
    fn operator_target<'a>(tag: &str, link: &'a ExternalImportLink) -> Option<&'a str> {
        if !link.matches_boundary("host") || !link.matches_platform_tag(tag) {
            return None;
        }
        match link.locator_segments() {
            [mnemonic] => Some(mnemonic.as_str()),
            _ => None,
        }
    }

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

        let (jvm_link, jvm) = select_unique_target(&links, |link| operator_target("jvm-operator", link))
            .expect("jvm 裁决唯一")
            .expect("jvm 绑定");
        assert_eq!(jvm, "integer.add");
        assert!(std::ptr::eq(jvm_link, &links[0]), "JVM 必须选中声明里的 jvm-operator 候选");

        let (clr_link, clr) = select_unique_target(&links, |link| operator_target("clr-operator", link))
            .expect("clr 裁决唯一")
            .expect("clr 绑定");
        assert_eq!(clr, "i32.add");
        assert!(std::ptr::eq(clr_link, &links[1]), "CLR 必须选中声明里的 clr-operator 候选");

        let (wasm_link, wasm) = select_unique_target(&links, wasm_operator_target).expect("wasm 裁决唯一").expect("wasm 绑定");
        assert_eq!(wasm, "i32.add");
        assert!(std::ptr::eq(wasm_link, &links[2]), "Wasm 必须选中声明里的 wasm-operator 候选");

        #[cfg(feature = "ovm-lane")]
        {
            let (ovm_link, ovm) = select_unique_target(&links, |link| operator_target("ovm-operator", link))
                .expect("ovm 裁决唯一")
                .expect("ovm 绑定");
            assert_eq!(ovm, "i32.add");
            assert!(std::ptr::eq(ovm_link, &links[3]), "OVM 必须选中声明里的 ovm-operator 候选");
        }

        assert!(
            select_unique_target(&links, |link| operator_target("x86_64-operator", link))
                .expect("x86_64 裁决唯一")
                .is_none(),
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
        let mut checks = vec![
            ("clr", select_unique_target(&links, |link| operator_target("clr-operator", link)).expect("clr 裁决唯一")),
            ("wasm", select_unique_target(&links, wasm_operator_target).expect("wasm 裁决唯一")),
            ("x86_64", select_unique_target(&links, |link| operator_target("x86_64-operator", link)).expect("x86_64 裁决唯一")),
        ];
        #[cfg(feature = "ovm-lane")]
        checks.push(("ovm", select_unique_target(&links, |link| operator_target("ovm", link)).expect("ovm 裁决唯一")));
        for (label, matched) in checks {
            assert!(matched.is_none(), "{label} 不得借用其他域的目标绑定");
        }
    }

    #[test]
    fn duplicate_binding_for_same_target_fails_closed() {
        let links = vec![
            ExternalImportLink::host(Some(Identifier::new("jvm-operator")), vec!["integer.add".into()]),
            ExternalImportLink::host(Some(Identifier::new("jvm-operator")), vec!["integer.subtract".into()]),
        ];

        let error = select_unique_target(&links, |link| operator_target("jvm-operator", link)).expect_err("重复目标绑定必须歧义失败，禁止 first-wins");
        assert!(error.contains("歧义"), "错误必须是歧义形态: {error}");
        assert!(error.contains("jvm-operator"), "错误必须指出命中的 platform tag: {error}");
        assert!(error.contains("integer.add") && error.contains("integer.subtract"), "错误必须列出全部候选: {error}");
    }
}
