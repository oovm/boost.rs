//! OVM 语言 × 目标适配器：导入链接分类与 operator 绑定的源语义裁决。
//!
//! 架构合同：链接分类（`[ovm_operator]` / `[ovm]` 助记符提取）、声明签名核对与
//! 冻结入口归语言侧适配器；emitter 的 OVM 后端只保留目标私有冻结表
//! （`freeze_ovm_operator_binding` / `OvmOperatorCode`）与编码原语。

use compiler_emitter::emitter_backend_vm::{OvmOperatorBinding, freeze_ovm_operator_binding};
use compiler_target::ExternalImportLink;
use compiler_types::{ItemInstanceId, SemanticType};

use crate::compiler::emitter::pipeline::operator;

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

/// 按声明的导入签名冻结 OVM operator 绑定。
pub(crate) fn freeze_ovm_operator(
    link: &ExternalImportLink,
    instance: ItemInstanceId,
    symbol: &str,
    parameter_types: &[SemanticType],
    return_type: &SemanticType,
) -> Result<Option<OvmOperatorBinding>, String> {
    let Some(mnemonic) = ovm_operator_target(link)
    else {
        return Ok(None);
    };
    let Some(plan) = freeze_ovm_operator_binding(mnemonic)
    else {
        return Ok(Some(OvmOperatorBinding::Unfrozen));
    };
    operator::validate_frozen_operator(mnemonic, instance, symbol, parameter_types, return_type, &plan)?;
    Ok(Some(OvmOperatorBinding::Frozen(plan)))
}

/// 整数表示形状：只携带位宽与符号性，`Character` 视为 32 位无符号码位。
///
/// 转换规则必须由这两项决定，不得按 `SemanticType` 变体名逐条特判。
#[derive(Clone, Copy)]
pub(crate) struct OvmIntShape {
    /// 位宽。
    pub bits: u16,
    /// 是否有符号。
    pub signed: bool,
}

/// 语义类型 → 整数表示形状；非整数形状返回 `None`。
pub(crate) fn ovm_int_shape(ty: &SemanticType) -> Option<OvmIntShape> {
    match ty {
        SemanticType::Integer8 { signed } => Some(OvmIntShape { bits: 8, signed: *signed }),
        SemanticType::Integer16 { signed } => Some(OvmIntShape { bits: 16, signed: *signed }),
        SemanticType::Integer32 { signed } => Some(OvmIntShape { bits: 32, signed: *signed }),
        SemanticType::Integer64 { signed } => Some(OvmIntShape { bits: 64, signed: *signed }),
        SemanticType::Character => Some(OvmIntShape { bits: 32, signed: false }),
        _ => None,
    }
}

#[cfg(test)]
mod wrapper_tests {
    use super::{freeze_ovm_operator, ovm_host_import_target, ovm_int_shape, ovm_operator_target};
    use compiler_emitter::emitter_backend_vm::OvmOperatorBinding;
    use compiler_target::{ExternalImportLink, Identifier};
    use compiler_types::{ItemInstanceId, SemanticType};

    fn link(mnemonic: &str) -> ExternalImportLink {
        ExternalImportLink::host(Some(Identifier::new("ovm-operator")), vec![mnemonic.to_string()])
    }

    #[test]
    fn link_mismatch_returns_none() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let foreign = ExternalImportLink::host(Some(Identifier::new("jvm-operator")), vec!["integer.add".into()]);
        assert_eq!(ovm_operator_target(&foreign), None);
        assert!(freeze_ovm_operator(&foreign, instance, "infix +", &[], &SemanticType::Unit).unwrap().is_none());
        assert_eq!(ovm_host_import_target(&link("i32.add")), None, "operator 域不得被宿主分类器借用");
    }

    #[test]
    fn matching_signature_freezes() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let i32_ty = SemanticType::Integer32 { signed: true };
        let binding = freeze_ovm_operator(&link("i32.add"), instance, "infix +", &[i32_ty.clone(), i32_ty.clone()], &i32_ty)
            .expect("冻结不得失败")
            .expect("必须是 OVM operator 绑定");
        let OvmOperatorBinding::Frozen(plan) = binding
        else {
            panic!("`i32.add` 必须冻结");
        };
        assert_eq!(plan.arity, 2);
    }

    #[test]
    fn unknown_mnemonic_stays_unfrozen() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let i64_ty = SemanticType::Integer64 { signed: true };
        let binding = freeze_ovm_operator(&link("i64.add"), instance, "infix +", &[i64_ty.clone(), i64_ty.clone()], &i64_ty)
            .expect("冻结不得失败")
            .expect("必须是 OVM operator 绑定");
        assert!(matches!(binding, OvmOperatorBinding::Unfrozen), "没有头码的助记符必须保持缺口");
    }

    #[test]
    fn arity_or_carrier_mismatch_fails() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let i32_ty = SemanticType::Integer32 { signed: true };
        let error = freeze_ovm_operator(&link("i32.add"), instance, "infix +", &[i32_ty.clone()], &i32_ty)
            .expect_err("arity 不符必须结构化失败");
        assert!(error.contains("arity"), "{error}");
        let i64_ty = SemanticType::Integer64 { signed: true };
        let error = freeze_ovm_operator(&link("i32.add"), instance, "infix +", &[i64_ty.clone(), i64_ty], &i32_ty)
            .expect_err("载体不符必须结构化失败");
        assert!(error.contains("载体"), "{error}");
    }

    #[test]
    fn int_shape_covers_integer_family_and_char() {
        let shape = ovm_int_shape(&SemanticType::Character).expect("`char` 必须投影为 32 位无符号码位");
        assert_eq!((shape.bits, shape.signed), (32, false));
        assert!(ovm_int_shape(&SemanticType::Float32).is_none());
    }
}
