//! `[when(...)]` / `[gate(...)]` 的语言侧条件语义：活动声明的判定归语言所有。
//!
//! 构建工具（Legion）拥有包图与源码快照，但**不得**自行解析 Valkyrie 前导 AST
//! 或另抄一份条件规则来决定文件是否参与构建。本模块是文件级与声明级条件的唯一
//! 实现：从已解析的 surface AST 提取条件 items、按目标/feature 集求值、判定文件
//! 活动性。消费端（planner 筛选、provider 选择、诊断）只调用这里的 API。
//!
//! `target.vendor` / `target.specification` 由 target registry 投影到四元组，不引入独立 platform 轴：
//! - `target.specification == "browser"` → Browser 规格（DOM / Web API）
//! - `target.vendor == "wechat"` → Wechat vendor（小程序 / 小游戏 runtime）
//! - `target.vendor == "node"` / `"deno"` / `"bun"` → 对应 JS runtime vendor
//! - 两者**不**互相兼容：vendor="wechat" 不会匹配 specification="browser"，反之亦然
//! - `target.specification == "wasip3"` / `"wasi"` → ABI / Wasi 规格
//!
//! canonical triple 拼写：wechat → `wasm32-wechat-unknown-wasm`（vendor=Wechat，specification=Unknown）。

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use compiler_target::{CanonicalAbi, CanonicalSpecification, CanonicalTarget, CanonicalVendor};
use data_text_valkyrie::lexer::token_type::ValkyrieTokenType;

use super::surface::{
    ast::{Attribute, QuoteLiteral, StatementNode, TermExpression},
    namespace_is_declaration_only, parse_source, parse_source_with_identity, ValkyrieRoot, ANONYMOUS_SOURCE_ID,
};

/// 单个 `[when(...)]` attribute item 归一后的目标/环境条件（不与其他 `when` 自动组合）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhenPredicate {
    /// 条件键（如 `vendor`、`specification`）。
    pub key: String,
    /// 条件值（字符串字面量）。
    pub value: String,
}

/// 单个 `[gate(...)]` attribute item 归一后的功能/构建条件（不与其他 `gate` 自动组合）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatePredicate {
    /// 条件键（第一版主要为 `feature`）。
    pub key: String,
    /// 条件值（字符串字面量）。
    pub value: String,
}

/// 文件级前导 `gate` / `when` items（各自独立列表，无跨 item 布尔语义）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileLevelConditionItems {
    pub gates: Vec<GatePredicate>,
    pub whens: Vec<WhenPredicate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateState {
    Enabled,
    Disabled,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhenState {
    Matched,
    Rejected,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WhenConditionKey {
    TargetSpecification,
    TargetVendor,
}

impl WhenPredicate {
    /// 从 surface AST 的 attribute 读取 `when` 条件；非 `when` 返回 `None`。
    pub fn from_when_attribute(attribute: &Attribute) -> Option<Self> {
        if attribute.name.name != "when" {
            return None;
        }
        let argument = attribute.args.first()?;
        if let Some(key) = &argument.key {
            let value = term_string_literal(&argument.value)?;
            if value.is_empty() {
                return None;
            }
            return Some(Self { key: key.name.clone(), value });
        }
        let (key, value) = parse_target_equality_expression(&argument.value)?;
        if value.is_empty() {
            return None;
        }
        Some(Self { key, value })
    }

    fn condition_key(&self) -> Option<WhenConditionKey> {
        match self.key.as_str() {
            "target_specification" | "specification" => Some(WhenConditionKey::TargetSpecification),
            "target_vendor" | "vendor" => Some(WhenConditionKey::TargetVendor),
            _ => None,
        }
    }
}

impl GatePredicate {
    /// `[gate(expression)]`：功能/构建开关条件（与 `when` 独立解析、独立记录）。
    ///
    /// 第一版仅解析并保存单条 `feature = "…"` 比较；不在此组合多个 `gate` item，也不与 `when` 求交。
    pub fn from_gate_attribute(attribute: &Attribute) -> Option<Self> {
        if attribute.name.name != "gate" {
            return None;
        }
        let argument = attribute.args.first()?;
        let key = argument.key.as_ref()?.name.as_str();
        let value = term_string_literal(&argument.value)?;
        if value.is_empty() {
            return None;
        }
        Some(Self { key: key.to_string(), value })
    }
}

/// attribute items 中全部 `when` 条件的提取（声明级与文件级共用）。
pub fn when_attributes_from_items(attributes: &[Attribute]) -> Vec<WhenPredicate> {
    attributes.iter().filter_map(WhenPredicate::from_when_attribute).collect()
}

/// attribute items 中全部 `gate` 条件的提取（声明级与文件级共用）。
pub fn gate_attributes_from_items(attributes: &[Attribute]) -> Vec<GatePredicate> {
    attributes.iter().filter_map(GatePredicate::from_gate_attribute).collect()
}

/// attribute 表达式位字符串字面量：仅 QuoteLiteral 可解码为常量文本，其余返回 `None`。
pub fn term_string_literal(expr: &TermExpression) -> Option<String> {
    match expr {
        TermExpression::QuoteLiteral(literal) => quote_capture_value(literal),
        _ => None,
    }
}

/// 常量位使用与表达式位同一套捕获解码；含插值即非常量，返回 `None`。
/// 宿主 attribute 的 `host_provider` 字符串参数读取共用本解码，故公开。
pub fn quote_capture_value(literal: &QuoteLiteral) -> Option<String> {
    use crate::compiler::hir::lowering::quote_literal::{DecodedQuoteCapture, decode_quote_capture};
    match decode_quote_capture(literal, false).ok()? {
        DecodedQuoteCapture::PlainText(text) => Some(text),
        DecodedQuoteCapture::Character(value) => Some(char::try_from(u32::from(value)).expect("char value is a valid scalar").to_string()),
        _ => None,
    }
}

/// `target.specification == "..."` 条件匹配。
/// 只匹配 CanonicalSpecification 字段，不再跨轴映射 vendor。
fn target_matches_specification_token(target: CanonicalTarget, token: &str) -> bool {
    match token.to_ascii_lowercase().as_str() {
        "browser-wasm" | "browser" | "web" => {
            target.specification == CanonicalSpecification::Browser && target.abi == Some(CanonicalAbi::WebAssembly)
        }
        "wasip3" | "wasi-p3" => target.abi == Some(CanonicalAbi::WasiP3),
        "wasi" | "wasip2" | "wasi-p2" => target.specification == CanonicalSpecification::Wasi && target.abi != Some(CanonicalAbi::WasiP3),
        _ => false,
    }
}

/// `target.vendor == "..."` 条件匹配。
/// 只匹配 CanonicalVendor 字段，不再把 vendor 值映射到 specification（那是历史错误）。
fn vendor_matches_token(target: CanonicalTarget, token: &str) -> bool {
    match token.to_ascii_lowercase().as_str() {
        "wechat" => target.vendor == CanonicalVendor::Wechat,
        "node" => matches!(target.vendor, CanonicalVendor::Node | CanonicalVendor::Deno | CanonicalVendor::Bun),
        "unknown" => target.vendor == CanonicalVendor::Unknown,
        _ => false,
    }
}

fn condition_atom_matches(target: CanonicalTarget, key: WhenConditionKey, value: &str) -> bool {
    match key {
        WhenConditionKey::TargetSpecification => target_matches_specification_token(target, value),
        WhenConditionKey::TargetVendor => vendor_matches_token(target, value),
    }
}

fn parse_target_equality_expression(expr: &TermExpression) -> Option<(String, String)> {
    let TermExpression::Binary(binary) = expr
    else {
        return None;
    };
    if binary.operator != ValkyrieTokenType::EqEq {
        return None;
    }
    let field = target_condition_field(&binary.lhs)?;
    let value = term_string_literal(&binary.rhs)?;
    Some((field, value))
}

fn target_condition_field(expr: &TermExpression) -> Option<String> {
    match expr {
        TermExpression::DotCall { receiver, field, .. } if is_target_receiver(receiver) => Some(field.name.clone()),
        _ => None,
    }
}

fn is_target_receiver(expr: &TermExpression) -> bool {
    matches!(expr, TermExpression::NamePath(path) if path.parts.len() == 1 && path.parts[0].name == "target")
}

/// 单个 `when` attribute item 是否匹配当前 target。
pub fn when_item_matches_target(predicate: &WhenPredicate, target: CanonicalTarget) -> bool {
    predicate.condition_key().map(|key| condition_atom_matches(target, key, &predicate.value)).unwrap_or(false)
}

/// 文件级 / provider 的 target 轴：仅处理 0 或 1 个 `when` item。
pub fn target_axis_passes(whens: &[WhenPredicate], target: CanonicalTarget) -> bool {
    match whens.len() {
        0 => true,
        1 => when_item_matches_target(&whens[0], target),
        _ => false,
    }
}

/// 文件级 / provider 的 gate 轴：0 条 gate 恒通过；有 gate 时仅当每条都被确认 `Enabled` 才通过。
///
/// fail-closed：`enabled_features == None`（无 feature 构建上下文）与未知 gate key 都得到
/// `Unknown`，按不通过处理；显式传入空集表示“未启用任何 feature”，因此所有 feature gate
/// 为 `Disabled`。
/// 多 gate 当前采取 "全部 Enabled 才通过" 的保守语义（避免隐式 OR）。
pub fn gate_axis_passes(gates: &[GatePredicate], enabled_features: Option<&BTreeSet<String>>) -> bool {
    gate_axis_state(gates, enabled_features) == GateState::Enabled
}

/// 聚合 gate 轴的诊断状态：任一 `Disabled` 立即为 `Disabled`；否则任一 `Unknown` 为 `Unknown`。
///
/// 只有空 gate 或全部 `Enabled` 才返回 `Enabled`。调用方不得把 `Unknown` 当作通过。
pub fn gate_axis_state(gates: &[GatePredicate], enabled_features: Option<&BTreeSet<String>>) -> GateState {
    let mut state = GateState::Enabled;
    for gate in gates {
        match evaluate_gate_item(gate, enabled_features) {
            GateState::Enabled => {}
            GateState::Disabled => return GateState::Disabled,
            GateState::Unknown => state = GateState::Unknown,
        }
    }
    state
}

pub fn evaluate_gate_item(gate: &GatePredicate, enabled_features: Option<&BTreeSet<String>>) -> GateState {
    match enabled_features {
        None => GateState::Unknown,
        Some(features) => {
            if gate.key != "feature" {
                return GateState::Unknown;
            }
            if features.contains(&gate.value) { GateState::Enabled } else { GateState::Disabled }
        }
    }
}

pub fn evaluate_when_item(when: &WhenPredicate, target: CanonicalTarget) -> WhenState {
    if when_item_matches_target(when, target) { WhenState::Matched } else { WhenState::Rejected }
}

/// parser assumes LF line endings. Normalize CRLF→LF before parsing so
/// Windows CI checkout (auto CRLF) and local (LF) behave identically.
fn normalize_line_endings(source: &str) -> String {
    source.replace("\r\n", "\n").replace('\r', "\n")
}

/// 解析源码并判定其文件级条件；无法解析时返回解析器原因，绝不静默排除。
/// `source_id` 是恢复诊断归属的源码身份：真实文件给逻辑路径，测试夹具给匿名占位。
pub fn source_file_conditions_pass(
    source: &str,
    target: CanonicalTarget,
    enabled_features: Option<&BTreeSet<String>>,
    source_id: &str,
) -> Result<bool, String> {
    let source = normalize_line_endings(source);
    let root = parse_source_with_identity(&source, source_id).map_err(|error| error.to_string())?;
    Ok(match file_level_condition_items_from_root(&root) {
        None => true,
        Some(items) => target_axis_passes(&items.whens, target) && gate_axis_passes(&items.gates, enabled_features),
    })
}

/// 读取源码并判断该文件是否参与当前 target + feature set 的语义图（无法解析按不活动处理）。
pub fn source_file_active_for_target_and_features(
    source: &str,
    target: CanonicalTarget,
    enabled_features: Option<&BTreeSet<String>>,
) -> bool {
    source_file_conditions_pass(source, target, enabled_features, ANONYMOUS_SOURCE_ID).unwrap_or(false)
}

/// 兼容旧签名：不传 feature 上下文时，无 gate 的文件照常通过；带 gate 的文件得到
/// `Unknown`，按 fail-closed 不参与构建。
pub fn source_file_active_for_target(source: &str, target: CanonicalTarget) -> bool {
    source_file_active_for_target_and_features(source, target, None)
}

pub fn file_level_condition_items(source: &str) -> Option<FileLevelConditionItems> {
    let source = normalize_line_endings(source);
    parse_source(&source).ok().and_then(|root| file_level_condition_items_from_root(&root))
}

/// `data.text` 将文件前导 attribute 挂在紧随其后的 `namespace …;` 的 `annotations` 上。
/// 仅当首个根项为分号式 namespace，且 annotations **仅含** `gate` / `when` 时视为文件级条件附着点。
pub fn file_level_condition_items_from_root(root: &ValkyrieRoot) -> Option<FileLevelConditionItems> {
    let first = root.items.first()?;
    let namespace = match first {
        StatementNode::Namespace(namespace) => namespace,
        _ => return None,
    };
    if !namespace_is_declaration_only(namespace) || namespace.annotations.is_empty() {
        return None;
    }
    for attribute in &namespace.annotations {
        match attribute.name.name.as_str() {
            "gate" | "when" => {}
            _ => return None,
        }
    }
    let gates = gate_attributes_from_items(&namespace.annotations);
    let whens = when_attributes_from_items(&namespace.annotations);
    if gates.is_empty() && whens.is_empty() {
        return None;
    }
    Some(FileLevelConditionItems { gates, whens })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feature_set(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn feature_gate(value: &str) -> GatePredicate {
        GatePredicate { key: "feature".into(), value: value.into() }
    }

    #[test]
    fn specification_browser_matches_only_browser_spec() {
        let browser = CanonicalTarget::browser();
        let wechat = CanonicalTarget::wechat();
        let spec_browser = WhenPredicate { key: "specification".into(), value: "browser".into() };
        assert!(when_item_matches_target(&spec_browser, browser));
        assert!(!when_item_matches_target(&spec_browser, wechat));
    }

    #[test]
    fn vendor_wechat_matches_only_wechat_vendor() {
        let browser = CanonicalTarget::browser();
        let wechat = CanonicalTarget::wechat();
        let vendor_wechat = WhenPredicate { key: "vendor".into(), value: "wechat".into() };
        assert!(when_item_matches_target(&vendor_wechat, wechat));
        assert!(!when_item_matches_target(&vendor_wechat, browser));
    }

    #[test]
    fn vendor_node_matches_js_runtimes() {
        let node = CanonicalTarget::node_wasm();
        let deno: CanonicalTarget = "wasm32-deno-unknown-wasm".parse().unwrap();
        let browser = CanonicalTarget::browser();
        let vendor_node = WhenPredicate { key: "vendor".into(), value: "node".into() };
        assert!(when_item_matches_target(&vendor_node, node));
        assert!(when_item_matches_target(&vendor_node, deno));
        assert!(!when_item_matches_target(&vendor_node, browser));
    }

    #[test]
    fn two_when_items_fail_target_axis_without_defined_combine_rule() {
        let whens = vec![
            WhenPredicate { key: "vendor".into(), value: "browser".into() },
            WhenPredicate { key: "vendor".into(), value: "wechat".into() },
        ];
        assert!(!target_axis_passes(&whens, CanonicalTarget::browser()));
    }

    #[test]
    fn gate_axis_unknown_without_feature_context_fails_closed() {
        let gates = vec![feature_gate("fetch")];
        assert_eq!(gate_axis_state(&gates, None), GateState::Unknown);
        assert!(!gate_axis_passes(&gates, None));
    }

    #[test]
    fn gate_axis_disabled_feature_is_not_enabled() {
        let gates = vec![feature_gate("fetch")];
        let enabled = feature_set(&[]);
        assert_eq!(gate_axis_state(&gates, Some(&enabled)), GateState::Disabled);
    }

    #[test]
    fn gate_axis_enabled_feature_passes() {
        let gates = vec![feature_gate("fetch")];
        let enabled = feature_set(&["fetch"]);
        assert_eq!(gate_axis_state(&gates, Some(&enabled)), GateState::Enabled);
    }

    #[test]
    fn gate_axis_non_feature_key_is_unknown() {
        let gates = vec![GatePredicate { key: "vendor".into(), value: "browser".into() }];
        let enabled = feature_set(&["fetch"]);
        assert_eq!(gate_axis_state(&gates, Some(&enabled)), GateState::Unknown);
    }

    #[test]
    fn empty_gate_axis_is_enabled_regardless_of_context() {
        assert_eq!(gate_axis_state(&[], None), GateState::Enabled);
        assert!(gate_axis_passes(&[], None));
    }

    #[test]
    fn mixed_gate_axis_reports_unknown_not_enabled() {
        let gates = vec![feature_gate("fetch"), GatePredicate { key: "vendor".into(), value: "browser".into() }];
        let enabled = feature_set(&["fetch"]);
        assert_eq!(gate_axis_state(&gates, Some(&enabled)), GateState::Unknown);
    }

    #[test]
    fn file_when_browser_file_inactive_on_wasi() {
        let source = r#"[when(target.specification == "browser")]
namespace std.adaptor.wasm;

[host_provider(std::math::random_next_i32)]
[js_builtin("random")]
micro js_random_next_i32(seed: i32): i32
"#;
        assert!(source_file_active_for_target(source, CanonicalTarget::browser()));
        assert!(!source_file_active_for_target(source, CanonicalTarget::wasi()));
    }

    #[test]
    fn file_without_leading_when_stays_active() {
        let source = r#"namespace std.adaptor.wasm;

[when(target.specification == "browser")]
[host_provider(std::math::random_next_i32)]
[js_builtin("random")]
micro js_random_next_i32(seed: i32): i32
"#;
        assert!(file_level_condition_items(source).is_none());
        assert!(source_file_active_for_target(source, CanonicalTarget::wasi()));
    }

    #[test]
    fn file_when_wasip3_inactive_on_browser_wasm() {
        let source = r#"[when(target.specification == "wasip3")]
namespace std.adaptor.wasi;
"#;
        let root = parse_source(source).expect("when file gate must parse");
        let items = file_level_condition_items_from_root(&root).expect("file-level when");
        assert_eq!(items.whens.len(), 1);
        assert!(!when_item_matches_target(&items.whens[0], CanonicalTarget::browser()));
        assert!(when_item_matches_target(&items.whens[0], CanonicalTarget::wasip3()));
        assert!(!source_file_active_for_target(source, CanonicalTarget::browser()));
        assert!(source_file_active_for_target(source, CanonicalTarget::wasip3()));
    }

    #[test]
    fn multiple_file_when_items_do_not_implicit_and() {
        let source = r#"[when(target.specification == "browser")]
[when(target.vendor == "wechat")]
namespace std.adaptor.wasm;
"#;
        assert!(!source_file_active_for_target(source, CanonicalTarget::browser()));
    }

    #[test]
    fn gate_and_when_coexist_without_combining_at_parse_layer() {
        let source = r#"[gate(feature = "fetch")]
[when(target.specification == "browser")]
namespace std.adaptor.wasm.fetch;
"#;
        let items = file_level_condition_items(source).expect("gate+when file");
        assert_eq!(items.gates.len(), 1);
        assert_eq!(items.gates[0].key, "feature");
        assert_eq!(items.gates[0].value, "fetch");
        assert_eq!(items.whens.len(), 1);
        let enabled = feature_set(&["fetch"]);
        assert!(source_file_active_for_target_and_features(source, CanonicalTarget::browser(), Some(&enabled)));
        assert!(!source_file_active_for_target_and_features(source, CanonicalTarget::wasi(), Some(&enabled)));
        // 未提供 feature 上下文 → Unknown → fail-closed 不参与构建。
        assert!(!source_file_active_for_target(source, CanonicalTarget::browser()));
    }

    #[test]
    fn file_level_gate_requires_explicit_feature_context() {
        let source = r#"[gate(feature = "fetch")]
namespace std.adaptor.wasm.fetch;
"#;
        let enabled = feature_set(&["fetch"]);
        let disabled = feature_set(&[]);
        assert!(source_file_active_for_target_and_features(source, CanonicalTarget::browser(), Some(&enabled)));
        assert!(!source_file_active_for_target_and_features(source, CanonicalTarget::browser(), Some(&disabled)));
        assert!(!source_file_active_for_target(source, CanonicalTarget::browser()));
    }

    #[test]
    fn file_level_unknown_gate_key_fails_closed() {
        let source = r#"[gate(vendor = "browser")]
namespace std.adaptor.wasm;
"#;
        let enabled = feature_set(&["fetch"]);
        assert!(!source_file_active_for_target_and_features(source, CanonicalTarget::browser(), Some(&enabled)));
        assert!(!source_file_active_for_target(source, CanonicalTarget::browser()));
    }
}
