//! CST formatter 往返：格式化归 `language.valkyrie`，解析复用 `data-text-valkyrie` 公开 API。
//!
//! 自 `data.text.valkyrie/src/parser/mod.rs` 随 formatter 迁出；断言不变。

use data_text_core::{Builder, SourceText, TextDiagnostics, parser::ParseSession};
use data_text_valkyrie::{
    ValkyrieBuilder, ValkyrieLanguage, ValkyrieRoot, ValkyrieTokenType,
    ast::{Statement, StatementNode, TemplateNode, TermExpression, Trait},
};
use language_valkyrie::formatter::{FormatOptions, format_source};

fn build(source: &str) -> TextDiagnostics<ValkyrieRoot> {
    let language = ValkyrieLanguage::default();
    let builder = ValkyrieBuilder::new(&language);
    let text = SourceText::new(source);
    let mut session = ParseSession::<ValkyrieLanguage>::default();
    builder.build(&text, &[], &mut session)
}

fn single_micro_body_expr(source: &str) -> TermExpression {
    let output = build(source);
    assert!(output.diagnostics.is_empty(), "diagnostics: {:?}", output.diagnostics);
    let root = output.result.unwrap_or_else(|error| panic!("expression must build, got {error:?}"));
    let micro = root
        .items
        .iter()
        .find_map(|item| match item {
            StatementNode::Micro(micro) => Some(micro),
            _ => None,
        })
        .expect("expected a micro item");
    let statement = micro.body.statements.first().expect("expected one body statement");
    match statement {
        Statement::Expression(expr) => expr.expr.clone(),
        other => panic!("expected expression statement, got {other:?}"),
    }
}

fn operator_of(expr: &TermExpression) -> Option<ValkyrieTokenType> {
    match expr {
        TermExpression::Binary(node) => Some(node.operator),
        _ => None,
    }
}

/// 与 std 消费端同构：trait 体内按 target 分派成员。
const TARGET_TRAIT: &str =
    "trait Iterator {\n    <% match arch %>\n    <% case wasm %>\n    micro filter_map(self, predicate: micro(i32) -> bool): i32\n    <% case native %>\n    micro take_while(self, predicate: micro(i32) -> bool): i32\n    <% end %>\n}\n";

fn root(source: &str) -> ValkyrieRoot {
    let output = build(source);
    assert!(output.diagnostics.is_empty(), "diagnostics: {:?}", output.diagnostics);
    output.result.unwrap_or_else(|error| panic!("trait templates must build, got {error:?}"))
}

fn trait_of<'root>(root: &'root ValkyrieRoot, name: &str) -> &'root Trait {
    root.items
        .iter()
        .find_map(|item| match item {
            StatementNode::Trait(trait_decl) if trait_decl.name.name == name => Some(trait_decl.as_ref()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected trait {name}"))
}

fn single_template<'root>(trait_decl: &'root Trait) -> &'root TemplateNode {
    assert_eq!(trait_decl.body.templates.len(), 1, "trait body must keep its template node: {:?}", trait_decl.body.templates);
    &trait_decl.body.templates[0]
}

fn member_names(items: &[StatementNode]) -> Vec<&str> {
    items
        .iter()
        .filter_map(|item| match item {
            StatementNode::Micro(micro) => Some(micro.name.name.as_str()),
            _ => None,
        })
        .collect()
}

/// 格式化往返：CST token-gap 格式化保留 `<=>`，重解析仍是三向比较算子。
#[test]
fn formatter_roundtrip_preserves_spaceship() {
    let formatted = format_source("micro f(){a<=>b}\n", &FormatOptions::default()).expect("formatting must succeed");
    assert!(formatted.contains("<=>"), "formatted output must keep the operator: {formatted:?}");
    let expr = single_micro_body_expr(&formatted);
    assert_eq!(operator_of(&expr), Some(ValkyrieTokenType::LessEqGreater), "reparsed expression: {expr:?}");
}

/// 格式化往返：CST token-gap 格式化保留 `.0`，重解析仍是数字成员 DotCall。
#[test]
fn formatter_roundtrip_preserves_tuple_index_member() {
    let formatted = format_source("micro f(){pair.0}\n", &FormatOptions::default()).expect("formatting must succeed");
    assert!(formatted.contains("pair.0"), "formatted output must keep the index member: {formatted:?}");
    let expr = single_micro_body_expr(&formatted);
    let TermExpression::DotCall { field, .. } = &expr
    else {
        panic!("reparsed expression must stay a member access, got {expr:?}")
    };
    assert_eq!(field.name, "0", "reparsed member name: {expr:?}");
}

/// 正例：CST 格式化往返保留模板原文，重解析仍得到同一批分支成员。
#[test]
fn formatter_roundtrip_preserves_trait_body_template() {
    let formatted = format_source(TARGET_TRAIT, &FormatOptions::default()).expect("formatting must succeed");
    assert!(formatted.contains("<% match arch %>"), "formatted output must keep the directive: {formatted:?}");
    let root = root(&formatted);
    let iterator = trait_of(&root, "Iterator");
    let TemplateNode::Match(matched) = single_template(iterator)
    else {
        panic!("reparsed trait must keep the match template: {:?}", iterator.body.templates)
    };
    assert_eq!(member_names(&matched.arms[0].body), ["filter_map"], "reparsed wasm arm: {:?}", matched.arms[0].body);
    assert_eq!(member_names(&matched.arms[1].body), ["take_while"], "reparsed native arm: {:?}", matched.arms[1].body);
}
