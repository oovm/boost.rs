//! `EngravingPlan` → `SemanticScene` 的确定性渲染。
//!
//! 渲染只把计划里的绝对几何翻译成语义节点，不重新排版、不度量文本、不产生
//! 后端指令。解析成显示几何是 `plotter_scene::SceneResolver` 的独立一步。

use plotter_scene::{
    PathData, Point, Rect, SemanticKind, SemanticNode, SemanticRole, SemanticScene, ShapeStyle, Size, SourceRef, TextAnchor, TextStyle,
};
use plotter_types::Color;

use crate::plan::{EngravingPlan, GlyphPlan, LinePlan, NotePlan, SystemPlan, TextPlan};

/// 谱表线颜色（随主题的描边色）。
const STAFF_STROKE: Color = Color::BLACK;

/// 把一张排版计划渲染成语义场景。
///
/// 节点结构：顶层是标题 / 作者文本，每行系统一个 `Region` 组，组内依次是谱表线、
/// 头部字形、音符组（`Node` 角色，带 `SourceRef` 指向 `measure.event` 标识）、
/// 符杠、小节线、弧线与连音组数字。
pub fn render(plan: &EngravingPlan) -> SemanticScene {
    let mut scene = SemanticScene::new(plan.canvas);

    if let Some(title) = &plan.title {
        scene.push(text_node(title, "title", SemanticRole::Title));
    }
    if let Some(composer) = &plan.composer {
        scene.push(text_node(composer, "composer", SemanticRole::Label));
    }

    for (index, system) in plan.systems.iter().enumerate() {
        scene.push(system_node(system, index));
    }
    scene
}

/// 一行谱表系统。
fn system_node(system: &SystemPlan, index: usize) -> SemanticNode {
    let mut children: Vec<SemanticNode> = Vec::new();

    // 谱表线：y 以系统内相对值计算，父组已带 origin 平移。
    for (line_index, line) in system.staff_lines().iter().enumerate() {
        children.push(line_segment_with_origin(line, system.origin, &format!("staff-{line_index}")));
    }

    for glyph in &system.header_glyphs {
        children.push(glyph_node(glyph, system.origin));
    }
    if let Some(glyph) = &system.tempo_glyph {
        children.push(glyph_node(glyph, system.origin));
    }
    if let Some(tempo) = &system.tempo {
        children.push(text_node_at(tempo, system.origin, "tempo"));
    }

    for note in &system.notes {
        children.push(note_node(note, system.origin));
    }

    for beam in &system.beams {
        for (band_index, band) in beam.bands.iter().enumerate() {
            let path = PathData::new()
                .move_to(relative(band[0], system.origin))
                .line_to(relative(band[1], system.origin))
                .line_to(relative(band[2], system.origin))
                .line_to(relative(band[3], system.origin))
                .close();
            children.push(
                SemanticNode::new(SemanticKind::Path { path, style: ShapeStyle::filled(STAFF_STROKE) })
                    .with_id(format!("beam-{index}-{band_index}"))
                    .with_role(SemanticRole::Decoration),
            );
        }
    }

    for (bar_index, bar) in system.barlines.iter().enumerate() {
        children.extend(barline_nodes(bar, system.origin, system.space, index, bar_index));
    }

    for dot in &system.repeat_dots {
        children.push(
            SemanticNode::new(SemanticKind::Ellipse {
                center: relative(dot.center, system.origin),
                radii: Size::new(dot.radius, dot.radius),
                style: ShapeStyle::filled(STAFF_STROKE),
            })
            .with_role(SemanticRole::Decoration),
        );
    }

    for (arc_index, arc) in system.arcs.iter().enumerate() {
        children.push(
            SemanticNode::new(SemanticKind::Path {
                path: shift_path(&arc.path, system.origin),
                style: ShapeStyle::stroked(STAFF_STROKE, arc.width),
            })
            .with_id(format!("arc-{index}-{arc_index}"))
            .with_role(SemanticRole::Decoration),
        );
    }

    for (number_index, number) in system.tuplet_numbers.iter().enumerate() {
        children.push(text_node_at(number, system.origin, &format!("tuplet-{index}-{number_index}")));
    }

    SemanticNode::group(children)
        .with_transform(plotter_types::Transform::new(1.0, 0.0, 0.0, 1.0, system.origin.x, system.origin.y))
        .with_id(format!("system-{index}"))
        .with_role(SemanticRole::Region)
}

/// 一个音符事件的全部几何。
fn note_node(note: &NotePlan, origin: Point) -> SemanticNode {
    let mut children: Vec<SemanticNode> = Vec::new();

    for line in &note.ledger_lines {
        children.push(line_segment_with_origin(line, origin, "ledger"));
    }
    if let Some(stem) = &note.stem {
        children.push(line_segment_with_origin(stem, origin, "stem"));
    }
    for dot in &note.articulation_dots {
        children.push(
            SemanticNode::new(SemanticKind::Ellipse {
                center: relative(dot.center, origin),
                radii: Size::new(dot.radius, dot.radius),
                style: ShapeStyle::filled(STAFF_STROKE),
            })
            .with_role(SemanticRole::Decoration),
        );
    }
    for line in &note.articulation_lines {
        children.push(line_segment_with_origin(line, origin, "articulation"));
    }
    for glyph in &note.accidentals {
        children.push(glyph_node(glyph, origin));
    }
    for glyph in &note.heads {
        children.push(glyph_node(glyph, origin));
    }
    for glyph in &note.flags {
        children.push(glyph_node(glyph, origin));
    }
    if let Some(lyric) = &note.lyric {
        let id =
            note.heads.first().and_then(|glyph| glyph.id.clone()).map(|head| format!("lyric-{head}")).unwrap_or_else(|| "lyric".to_string());
        children.push(text_node_at(lyric, origin, &id));
    }

    let node = SemanticNode::group(children).with_role(SemanticRole::Node);
    match note.heads.first().and_then(|glyph| glyph.id.clone()) {
        Some(id) => {
            let source = format!("event@{id}");
            node.with_id(id).with_source(SourceRef::element(source))
        }
        None => node,
    }
}

/// SMuFL 字形放置。
fn glyph_node(glyph: &GlyphPlan, origin: Point) -> SemanticNode {
    let mut node = SemanticNode::new(SemanticKind::Text {
        position: relative(glyph.at, origin),
        text: glyph.glyph.to_string(),
        style: glyph.style.clone(),
    })
    .with_role(SemanticRole::Decoration);
    if let Some(id) = &glyph.id {
        node = node.with_id(id);
    }
    node
}

/// 系统内文本。
fn text_node_at(text: &TextPlan, origin: Point, id: &str) -> SemanticNode {
    SemanticNode::new(SemanticKind::Text { position: relative(text.at, origin), text: text.text.clone(), style: text.style.clone() })
        .with_id(id.to_string())
        .with_role(SemanticRole::Label)
}

/// 画布级文本（标题 / 作者）。
fn text_node(text: &TextPlan, id: &str, role: SemanticRole) -> SemanticNode {
    SemanticNode::new(SemanticKind::Text { position: text.at, text: text.text.clone(), style: text.style.clone() })
        .with_id(id.to_string())
        .with_role(role)
}

/// 小节线。谱表高 4 个间距，粗线用于终止与反复。
fn barline_nodes(bar: &crate::plan::BarlinePlan, origin: Point, space: f64, system: usize, index: usize) -> Vec<SemanticNode> {
    use crate::spec::Barline;

    let thin = 0.12 * space;
    let thick = 0.5 * space;
    let height = 4.0 * space;

    let make = |x: f64, width: f64, id: String| {
        SemanticNode::new(SemanticKind::Rect { rect: Rect::new(x - origin.x, 0.0, width, height), style: ShapeStyle::filled(STAFF_STROKE) })
            .with_id(id)
            .with_role(SemanticRole::Decoration)
    };

    match bar.kind {
        Barline::Regular => vec![make(bar.x, thin, format!("barline-{system}-{index}"))],
        Barline::Final => vec![
            make(bar.x - 0.2 * space, thin, format!("barline-{system}-{index}-a")),
            make(bar.x, thick, format!("barline-{system}-{index}-b")),
        ],
        Barline::RepeatStart => vec![
            make(bar.x, thick, format!("barline-{system}-{index}-a")),
            make(bar.x + 0.2 * space, thin, format!("barline-{system}-{index}-b")),
        ],
        Barline::RepeatEnd => vec![
            make(bar.x - 0.2 * space, thin, format!("barline-{system}-{index}-a")),
            make(bar.x, thick, format!("barline-{system}-{index}-b")),
        ],
    }
}

/// 线段（计划坐标为绝对坐标，节点坐标取相对父组）。
fn line_segment_with_origin(line: &LinePlan, origin: Point, id: &str) -> SemanticNode {
    let mut node = SemanticNode::new(SemanticKind::Line {
        from: relative(line.from, origin),
        to: relative(line.to, origin),
        style: ShapeStyle::stroked(STAFF_STROKE, line.width),
    })
    .with_role(SemanticRole::Decoration);
    if let Some(line_id) = &line.id {
        node = node.with_id(line_id.clone());
    }
    else {
        node = node.with_id(id.to_string());
    }
    node
}

/// 绝对坐标 → 父组内相对坐标。
fn relative(point: Point, origin: Point) -> Point {
    Point::new(point.x - origin.x, point.y - origin.y)
}

/// 平移一条路径到父组坐标。
fn shift_path(path: &PathData, origin: Point) -> PathData {
    use plotter_scene::PathCommand;

    let mut shifted = PathData::new();
    shifted.commands = path
        .commands
        .iter()
        .map(|command| match *command {
            PathCommand::MoveTo(point) => PathCommand::MoveTo(relative(point, origin)),
            PathCommand::LineTo(point) => PathCommand::LineTo(relative(point, origin)),
            PathCommand::QuadraticTo { control, to } => {
                PathCommand::QuadraticTo { control: relative(control, origin), to: relative(to, origin) }
            }
            PathCommand::CubicTo { control1, control2, to } => {
                PathCommand::CubicTo { control1: relative(control1, origin), control2: relative(control2, origin), to: relative(to, origin) }
            }
            PathCommand::Close => PathCommand::Close,
        })
        .collect();
    shifted
}

/// 默认正文字体样式（供测试与宿主覆盖）。
pub fn body_style(size: f64, anchor: TextAnchor) -> TextStyle {
    TextStyle { fill: Some(Color::BLACK), opacity: 1.0, font_family: None, font_size: size, font_weight: None, text_anchor: anchor }
}
