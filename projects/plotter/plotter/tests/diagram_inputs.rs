//! 活跃输入冒烟：DOT、Mermaid、PlantUML 各走一次真实解析、布局和 SVG 序列化。
//!
//! 这三个前端是当前活跃输入，期望按真实渲染结果写，不写成拒绝。完整的
//! 语法矩阵和像素基准由 TS / harness 侧消费。

use plotter::input::{dot, mermaid, plantuml};

#[test]
fn dot_lowers_a_labeled_digraph() {
    let source = r#"digraph pipeline {
  rankdir=LR;
  A [label="Start", shape=circle];
  B [label="Stop"];
  A -> B;
}"#;

    let lowered = dot::dot_to_scene(source).expect("the DOT subset should render");

    assert_eq!(lowered.plan.nodes.len(), 2);
    assert_eq!(lowered.plan.edges.len(), 1);

    let svg = lowered.scene.to_svg().expect("the DOT scene should serialize");
    assert!(svg.contains(">Start</text>"));
    assert!(svg.contains(">Stop</text>"));
}

#[test]
fn mermaid_lowers_a_branching_flowchart() {
    let source = "flowchart TD\n  A[Start] --> B{Ready?}\n  B -->|yes| C[Stop]\n  B -->|no| D[Retry]\n";

    let lowered = mermaid::mermaid_to_scene(source).expect("the flowchart subset should render");

    assert_eq!(lowered.plan.nodes.len(), 4);
    assert_eq!(lowered.plan.edges.len(), 3);
    assert_eq!(lowered.plan.node("A").expect("A").label, "Start");

    let svg = lowered.scene.to_svg().expect("the Mermaid scene should serialize");
    assert!(svg.contains(">Retry</text>"));
}

#[test]
fn plantuml_lowers_a_class_definition() {
    let source = "@startuml\nclass Order {\n  +id : String\n}\n@enduml\n";

    let lowered = plantuml::plantuml_to_scene(source).expect("the class subset should render");

    assert_eq!(lowered.plan.nodes.len(), 1);
    assert_eq!(lowered.plan.node("Order").expect("Order").label, "Order\n+id : String");

    let svg = lowered.scene.to_svg().expect("the PlantUML scene should serialize");
    assert!(svg.contains("Order"));
}
