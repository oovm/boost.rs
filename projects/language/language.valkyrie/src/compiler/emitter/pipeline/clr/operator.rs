//! CLR 语言 × 目标适配器：导入链接分类与 operator 绑定的源语义裁决。
//!
//! 架构合同：链接分类（`[clr_operator]` 助记符提取）与声明签名核对归语言侧
//! 适配器；emitter 的 CLR 后端只保留助记符到 MSIL 配方的目标私有冻结表
//! （`freeze_clr_operator_binding`）与编码展开。

use compiler_emitter::emitter_backend_clr::{ClrOperatorBinding, freeze_clr_operator_binding};
use compiler_target::ExternalImportLink;
use compiler_types::{ItemInstanceId, SemanticType};

use crate::compiler::emitter::pipeline::operator;

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

/// 按声明的导入签名冻结 CLR operator 绑定。
pub(crate) fn freeze_clr_operator(
    link: &ExternalImportLink,
    instance: ItemInstanceId,
    symbol: &str,
    parameter_types: &[SemanticType],
    return_type: &SemanticType,
) -> Result<Option<ClrOperatorBinding>, String> {
    let Some(mnemonic) = clr_operator_target(link)
    else {
        return Ok(None);
    };
    let Some(plan) = freeze_clr_operator_binding(mnemonic)
    else {
        return Ok(Some(ClrOperatorBinding::Unfrozen));
    };
    operator::validate_frozen_operator(mnemonic, instance, symbol, parameter_types, return_type, &plan)?;
    Ok(Some(ClrOperatorBinding::Frozen(plan)))
}

#[cfg(test)]
mod wrapper_tests {
    use super::{clr_operator_target, freeze_clr_operator};
    use compiler_emitter::emitter_backend_clr::{ClrOperatorBinding, ClrOperatorCode, MsilOpcode};
    use compiler_target::{ExternalImportLink, Identifier};
    use compiler_types::{ItemInstanceId, SemanticType};

    fn link(mnemonic: &str) -> ExternalImportLink {
        ExternalImportLink::host(Some(Identifier::new("clr-operator")), vec![mnemonic.to_string()])
    }

    #[test]
    fn link_mismatch_returns_none() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let foreign = ExternalImportLink::host(Some(Identifier::new("jvm-operator")), vec!["integer.add".into()]);
        assert_eq!(clr_operator_target(&foreign), None);
        assert!(freeze_clr_operator(&foreign, instance, "infix +", &[], &SemanticType::Unit).unwrap().is_none());
    }

    #[test]
    fn matching_signature_freezes() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let i32_ty = SemanticType::Integer32 { signed: true };
        let binding = freeze_clr_operator(&link("i32.add"), instance, "infix +", &[i32_ty.clone(), i32_ty.clone()], &i32_ty)
            .expect("冻结不得失败")
            .expect("必须是 CLR operator 绑定");
        let ClrOperatorBinding::Frozen(plan) = binding
        else {
            panic!("`i32.add` 必须冻结");
        };
        assert_eq!(plan.arity, 2);
        assert_eq!(plan.encoding, ClrOperatorCode::Single(MsilOpcode::Add));
    }

    #[test]
    fn unknown_mnemonic_stays_unfrozen() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let f64_ty = SemanticType::Float64;
        let binding = freeze_clr_operator(&link("f64.rem.unknown"), instance, "infix %", &[f64_ty.clone(), f64_ty.clone()], &f64_ty)
            .expect("冻结不得失败")
            .expect("必须是 CLR operator 绑定");
        assert!(matches!(binding, ClrOperatorBinding::Unfrozen), "近似但未登记的助记符不得被猜成 opcode");
    }

    #[test]
    fn arity_or_carrier_mismatch_fails() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let i32_ty = SemanticType::Integer32 { signed: true };
        let error = freeze_clr_operator(&link("i32.add"), instance, "infix +", &[i32_ty.clone()], &i32_ty).expect_err("arity 不符必须结构化失败");
        assert!(error.contains("arity"), "{error}");
        let i64_ty = SemanticType::Integer64 { signed: true };
        let error = freeze_clr_operator(&link("i32.add"), instance, "infix +", &[i64_ty.clone(), i64_ty], &i32_ty).expect_err("载体不符必须结构化失败");
        assert!(error.contains("载体"), "{error}");
    }
}
