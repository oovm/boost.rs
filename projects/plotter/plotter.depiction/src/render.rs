//! `DepictionPlan` → `SemanticScene` 的确定性渲染。
//!
//! 只翻译计划几何，不重新布局、不度量文本、不产生后端指令。

use plotter_scene::{Rect, SemanticKind, SemanticNode, SemanticRole, SemanticScene, ShapeStyle, Size, TextAnchor, TextStyle};
use plotter_types::{Color, Point};

use crate::plan::{ArrowPlan, DepictionPlan, LabelPlan, LinePlan};

/// 骨架与箭头颜色。
const SKELETON: Color = Color::BLACK;
/// 标签背景色。
const BACKDROP: Color = Color::WHITE;

/// 把描绘计划渲染成语义场景。
pub fn render(plan: &DepictionPlan) -> SemanticScene {
    let mut scene = SemanticScene::new(plan.canvas);

    for (index, line) in plan.lines.iter().enumerate() {
        scene.push(line_node(line, &format!("bond-{index}")));
    }

    for (index, arrow) in plan.arrows.iter().enumerate() {
        for (part, node) in arrow_nodes(arrow, index) {
            scene.push(node.with_id(part));
        }
    }

    for (index, label) in plan.labels.iter().enumerate() {
        scene.push(label_node(label, &format!("label-{index}")));
    }

    scene
}

/// 一条键线。
fn line_node(line: &LinePlan, id: &str) -> SemanticNode {
    let mut style = ShapeStyle::stroked(line.color.unwrap_or(SKELETON), line.width);
    if line.dashed {
        style.dash = Some([2.0 * line.width, 2.0 * line.width]);
    }
    SemanticNode::new(SemanticKind::Line { from: line.from, to: line.to, style }).with_id(id.to_string()).with_role(SemanticRole::Edge)
}

/// 标签：可选背景块 + 文本。
fn label_node(label: &LabelPlan, id: &str) -> SemanticNode {
    let mut children = Vec::new();
    if label.backdrop {
        let width = 0.62 * label.size * label.text.chars().count().max(1) as f64;
        let height = 1.1 * label.size;
        let (x, y) = match label.anchor {
            TextAnchor::Start => (label.at.x, label.at.y - 0.8 * label.size),
            TextAnchor::Middle => (label.at.x - width * 0.5, label.at.y - 0.8 * label.size),
            TextAnchor::End => (label.at.x - width, label.at.y - 0.8 * label.size),
        };
        children.push(
            SemanticNode::new(SemanticKind::Rect { rect: Rect::new(x, y, width, height), style: ShapeStyle::filled(BACKDROP) })
                .with_role(SemanticRole::EdgeLabelBackdrop),
        );
    }
    children.push(
        SemanticNode::new(SemanticKind::Text {
            position: label.at,
            text: label.text.clone(),
            style: TextStyle {
                fill: Some(label.color.unwrap_or(Color::BLACK)),
                opacity: 1.0,
                font_family: None,
                font_size: label.size,
                font_weight: None,
                text_anchor: label.anchor,
            },
        })
        .with_role(SemanticRole::Label),
    );
    SemanticNode::group(children).with_id(id.to_string()).with_role(SemanticRole::Label)
}

/// 箭头：杆、箭头头部与条件文本。
fn arrow_nodes(arrow: &ArrowPlan, index: usize) -> Vec<(String, SemanticNode)> {
    let mut nodes = Vec::new();
    let width = 0.05 * (arrow.from.distance(arrow.to) / 4.0).max(1.0);
    // 箭头头部按杆长取 6%，并夹在 4–10 个场景单位之间：固定比例会在长箭头上
    // 缩成几乎看不见的小三角，短箭头上又过大。
    let head = (0.06 * arrow.from.distance(arrow.to)).clamp(4.0, 10.0);

    let head_at = |tip: Point, base: Point| {
        let dx = tip.x - base.x;
        let dy = tip.y - base.y;
        let len = dx.hypot(dy).max(1e-9);
        let unit = Point::new(dx / len, dy / len);
        let normal = Point::new(-unit.y, unit.x);
        let wing = Point::new(tip.x - unit.x * head, tip.y - unit.y * head);
        [
            Point::new(wing.x + normal.x * head * 0.42, wing.y + normal.y * head * 0.42),
            tip,
            Point::new(wing.x - normal.x * head * 0.42, wing.y - normal.y * head * 0.42),
        ]
    };

    match arrow.kind {
        crate::spec::ArrowKind::Equilibrium => {
            // 双向：两条平行半箭头。
            let normal = Point::new(-(arrow.to.y - arrow.from.y), arrow.to.x - arrow.from.x);
            let len = normal.x.hypot(normal.y).max(1e-9);
            let shift = Point::new(normal.x / len * head * 0.35, normal.y / len * head * 0.35);
            let upper_from = Point::new(arrow.from.x + shift.x, arrow.from.y + shift.y);
            let upper_to = Point::new(arrow.to.x + shift.x, arrow.to.y + shift.y);
            let lower_from = Point::new(arrow.from.x - shift.x, arrow.from.y - shift.y);
            let lower_to = Point::new(arrow.to.x - shift.x, arrow.to.y - shift.y);
            nodes.push((
                format!("arrow-{index}-shaft-a"),
                SemanticNode::new(SemanticKind::Line { from: upper_from, to: upper_to, style: ShapeStyle::stroked(SKELETON, width) })
                    .with_role(SemanticRole::Edge),
            ));
            nodes.push((format!("arrow-{index}-head-a"), head_node(head_at(upper_to, upper_from))));
            nodes.push((
                format!("arrow-{index}-shaft-b"),
                SemanticNode::new(SemanticKind::Line { from: lower_to, to: lower_from, style: ShapeStyle::stroked(SKELETON, width) })
                    .with_role(SemanticRole::Edge),
            ));
            nodes.push((format!("arrow-{index}-head-b"), head_node(head_at(lower_from, lower_to))));
        }
        crate::spec::ArrowKind::Single | crate::spec::ArrowKind::Double => {
            nodes.push((
                format!("arrow-{index}-shaft"),
                SemanticNode::new(SemanticKind::Line { from: arrow.from, to: arrow.to, style: ShapeStyle::stroked(SKELETON, width) })
                    .with_role(SemanticRole::Edge),
            ));
            nodes.push((format!("arrow-{index}-head"), head_node(head_at(arrow.to, arrow.from))));
            if matches!(arrow.kind, crate::spec::ArrowKind::Double) {
                nodes.push((format!("arrow-{index}-head-b"), head_node(head_at(arrow.to, arrow.from))));
            }
        }
    }

    if let Some(above) = &arrow.above {
        nodes.push((format!("arrow-{index}-above"), label_node(above, &format!("arrow-{index}-above-text"))));
    }
    if let Some(below) = &arrow.below {
        nodes.push((format!("arrow-{index}-below"), label_node(below, &format!("arrow-{index}-below-text"))));
    }
    nodes
}

fn head_node(points: [Point; 3]) -> SemanticNode {
    let path = plotter_scene::PathData::new().move_to(points[0]).line_to(points[1]).line_to(points[2]).close();
    SemanticNode::new(SemanticKind::Path { path, style: ShapeStyle::filled(SKELETON) }).with_role(SemanticRole::Edge)
}

/// 画布尺寸辅助（测试与宿主用）。
pub fn scene_bounds(plan: &DepictionPlan) -> Size {
    plan.canvas
}
