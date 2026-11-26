//! JVM 语言 × 目标适配器：导入链接分类与 operator 绑定的源语义裁决。
//!
//! 架构合同：链接分类（`[jvm_operator]` 助记符提取）与声明签名核对归语言侧
//! 适配器；emitter 的 JVM 后端只保留助记符到冻结配方的目标私有配方表
//! （`freeze_jvm_operator_binding`）与编码展开。

use compiler_emitter::emitter_backend_jvm::{JvmOperatorBinding, freeze_jvm_operator_binding};
use compiler_target::ExternalImportLink;
use compiler_types::{ItemInstanceId, SemanticType};

use crate::compiler::emitter::pipeline::operator;

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

/// 按声明的导入签名冻结 JVM operator 绑定。
///
/// `None` 表示该导入不是 JVM operator；`Some(Unfrozen)` 表示绑定了 JVM operator
/// 但名字不在本 lane 的配方表内。两者都必须保留，禁止回落为宿主导入或默认成功。
pub(crate) fn freeze_jvm_operator(
    link: &ExternalImportLink,
    instance: ItemInstanceId,
    symbol: &str,
    parameter_types: &[SemanticType],
    return_type: &SemanticType,
) -> Result<Option<JvmOperatorBinding>, String> {
    let Some(mnemonic) = jvm_operator_target(link)
    else {
        return Ok(None);
    };
    let Some(plan) = freeze_jvm_operator_binding(mnemonic)
    else {
        return Ok(Some(JvmOperatorBinding::Unfrozen));
    };
    operator::validate_frozen_operator(mnemonic, instance, symbol, parameter_types, return_type, &plan)?;
    Ok(Some(JvmOperatorBinding::Frozen(plan)))
}

#[cfg(test)]
mod wrapper_tests {
    use super::{freeze_jvm_operator, jvm_operator_target};
    use compiler_emitter::emitter_backend_jvm::{JvmOperatorBinding, JvmOperatorCode};
    use compiler_target::{ExternalImportLink, Identifier};
    use compiler_types::{ItemInstanceId, SemanticType};

    fn link(mnemonic: &str) -> ExternalImportLink {
        ExternalImportLink::host(Some(Identifier::new("jvm-operator")), vec![mnemonic.to_string()])
    }

    #[test]
    fn link_mismatch_returns_none() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let foreign = ExternalImportLink::host(Some(Identifier::new("clr-operator")), vec!["i32.add".into()]);
        assert_eq!(jvm_operator_target(&foreign), None);
        assert!(freeze_jvm_operator(&foreign, instance, "infix +", &[], &SemanticType::Unit).unwrap().is_none());
    }

    #[test]
    fn matching_signature_freezes() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let i32_ty = SemanticType::Integer32 { signed: true };
        let binding = freeze_jvm_operator(&link("integer.add"), instance, "infix +", &[i32_ty.clone(), i32_ty.clone()], &i32_ty)
            .expect("冻结不得失败")
            .expect("必须是 JVM operator 绑定");
        let JvmOperatorBinding::Frozen(plan) = binding
        else {
            panic!("`integer.add` 必须冻结");
        };
        assert_eq!(plan.arity, 2);
        assert_eq!(plan.encoding, JvmOperatorCode::IntegerAdd);
    }

    #[test]
    fn unknown_mnemonic_stays_unfrozen() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let binding = freeze_jvm_operator(&link("long.div.un"), instance, "infix /", &[SemanticType::Integer64 { signed: false }, SemanticType::Integer64 { signed: false }], &SemanticType::Integer64 { signed: false })
            .expect("冻结不得失败")
            .expect("必须是 JVM operator 绑定");
        assert!(matches!(binding, JvmOperatorBinding::Unfrozen), "近似但未登记的助记符不得被猜成指令");
    }

    #[test]
    fn arity_or_carrier_mismatch_fails() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let i32_ty = SemanticType::Integer32 { signed: true };
        let error = freeze_jvm_operator(&link("integer.add"), instance, "infix +", &[i32_ty.clone()], &i32_ty).expect_err("arity 不符必须结构化失败");
        assert!(error.contains("arity"), "{error}");
        let i64_ty = SemanticType::Integer64 { signed: true };
        let error = freeze_jvm_operator(&link("integer.add"), instance, "infix +", &[i64_ty.clone(), i64_ty], &i32_ty).expect_err("载体不符必须结构化失败");
        assert!(error.contains("载体"), "{error}");
    }
}
