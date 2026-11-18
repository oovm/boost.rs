//! 图表冒烟：最小 happy path 与一条关键拒绝路径。
//!
//! 真实覆盖矩阵和图像基准由 TS / harness 侧消费，这里只证明上游 crate
//! 在本仓库内可以编译、编译出 plan 并序列化 SVG。

use plotter::{
    chart::{Channel, ChannelEncoding, ChartOptions, ChartSpec, DataColumn, DataSnapshot, FieldKind, FieldRef, Mark, Value, codes, lower},
    scene::SceneResolver,
};

fn snapshot() -> DataSnapshot {
    DataSnapshot::new(vec![
        DataColumn::new("category", FieldKind::Nominal, vec![Value::text("A"), Value::text("B"), Value::text("C")]),
        DataColumn::new("value", FieldKind::Quantitative, vec![Value::number(10.0), Value::number(20.0), Value::number(5.0)]),
    ])
}

fn bar_spec() -> ChartSpec {
    ChartSpec::new(Mark::Bar)
        .with_field(FieldRef::new("category", FieldKind::Nominal))
        .with_field(FieldRef::new("value", FieldKind::Quantitative))
        .with_encoding(Channel::X, ChannelEncoding::field("category", FieldKind::Nominal))
        .with_encoding(Channel::Y, ChannelEncoding::field("value", FieldKind::Quantitative))
}

#[test]
fn bar_chart_lowers_to_a_display_scene_and_svg() {
    let lowered = lower(&bar_spec(), &snapshot(), &ChartOptions::default()).expect("a complete bar chart should compile");

    assert_eq!(lowered.plan.marks.len(), 3, "one mark instance per observation");
    assert!(lowered.diagnostics.is_empty(), "a complete bar chart must not report loss");

    let display = lowered.scene.resolve(&SceneResolver::new());
    let svg = display.to_svg().expect("a resolved chart scene should serialize");
    assert!(svg.starts_with("<svg"), "the serializer must emit an SVG document");
    assert!(svg.contains("<rect"), "bars are emitted as rect nodes");
    assert!(svg.contains("<text"), "axis labels are emitted as text nodes");
}

#[test]
fn an_undeclared_field_is_rejected_with_a_structured_code() {
    let spec = ChartSpec::new(Mark::Point)
        .with_encoding(Channel::X, ChannelEncoding::field("missing", FieldKind::Quantitative))
        .with_encoding(Channel::Y, ChannelEncoding::field("missing", FieldKind::Quantitative));

    let error = lower(&spec, &snapshot(), &ChartOptions::default()).expect_err("undeclared fields must be refused");

    assert_eq!(error.code(), codes::UNKNOWN_FIELD);
}
