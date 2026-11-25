//! 从 `data.text` `Attribute` 读取 planner 事实（不渲染 attribute 文本、不做二次 parse）。
//!
//! `[when]` / `[gate]` 条件语义与字符串字面量提取归语言侧
//! `language_valkyrie::compiler::syntax::conditions` 所有，本模块只再导出供 inventory
//! 使用；这里保留的是宿主合同 attribute（`[host_contract]` / `[foreign_binding]` /
//! `[host_provider]`）的读取，属于包图装配而非条件语义。

use language_valkyrie::compiler::syntax::surface::ast::{Attribute, TermExpression};

use super::capability::ForeignBinding;

pub(crate) use language_valkyrie::compiler::syntax::conditions::{gate_attributes_from_items, term_string_literal, when_attributes_from_items};

pub(crate) fn has_host_contract_attribute(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| attribute.name.name == "host_contract")
}

pub(crate) fn foreign_bindings_from_attributes(attributes: &[Attribute]) -> Vec<ForeignBinding> {
    attributes.iter().filter_map(foreign_binding_from_attribute).collect()
}

fn foreign_binding_from_attribute(attribute: &Attribute) -> Option<ForeignBinding> {
    let name = attribute.name.name.as_str();
    match name {
        "js_builtin" => {
            let symbol = attribute.args.first().and_then(|arg| term_string_literal(&arg.value))?;
            Some(ForeignBinding::JsBuiltin { symbol })
        }
        "wasm" => {
            let module = attribute.args.first().and_then(|arg| term_string_literal(&arg.value))?;
            let field = attribute.args.get(1).and_then(|arg| term_string_literal(&arg.value))?;
            Some(ForeignBinding::Wasm { module, field })
        }
        "clr" => {
            let symbol = attribute.args.first().and_then(|arg| term_string_literal(&arg.value))?;
            Some(ForeignBinding::Clr { symbol })
        }
        "jvm" => {
            let symbol = attribute.args.first().and_then(|arg| term_string_literal(&arg.value))?;
            Some(ForeignBinding::Jvm { symbol })
        }
        _ => None,
    }
}

pub(crate) fn host_provider_contract_targets(attributes: &[Attribute], source: &str) -> Vec<String> {
    attributes
        .iter()
        .filter(|attribute| attribute.name.name == "host_provider")
        .filter_map(|attribute| attribute.args.first())
        .filter_map(|argument| host_provider_contract_reference(&argument.value, source))
        .collect()
}

/// `host_provider` 参数：字符串字面量走 AST 解码，其余用表达式 span 在源码中的切片（不拼 attribute 文本）。
pub(crate) fn host_provider_contract_reference(expr: &TermExpression, source: &str) -> Option<String> {
    match expr {
        TermExpression::QuoteLiteral(literal) => language_valkyrie::compiler::syntax::conditions::quote_capture_value(literal),
        expr => {
            let span = expr.span();
            source.get(span.start..span.end).map(str::trim).filter(|text| !text.is_empty()).map(str::to_string)
        }
    }
}
