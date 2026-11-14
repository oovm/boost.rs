//! 确定性描绘结果。
//!
//! [`DepictionPlan`] 只携带几何与文本意图，不携带 SVG 字符串或渲染指令。

use plotter_scene::TextAnchor;
use plotter_types::{Color, Diagnostic, Point, Size};

use crate::spec::ArrowKind;

/// 描绘调参。
#[derive(Clone, Debug, PartialEq)]
pub struct DepictionOptions {
    /// 键长（场景单位）。
    pub bond_length: f64,
    /// 页边距（场景单位）。
    pub margin: f64,
    /// 配色：单色墨线或 CPK 元素配色。
    pub palette: Palette,
}

impl Default for DepictionOptions {
    fn default() -> Self {
        Self { bond_length: 48.0, margin: 24.0, palette: Palette::Element }
    }
}

/// 描绘配色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Palette {
    /// 黑色墨线 + 黑色标签，适合印刷与单色文档。
    Ink,
    /// 元素配色：键保持黑色细线，杂原子标签按元素着色（氧红、氮蓝、硫黄、
    /// 卤素绿紫）。这是 ChemDraw / PubChem / Wolfram `MoleculePlot` 的
    /// 2D 通行的读图习惯。
    Element,
}

/// 一条线（键、键的多重线、箭头杆）。
#[derive(Clone, Debug, PartialEq)]
pub struct LinePlan {
    /// 起点。
    pub from: Point,
    /// 终点。
    pub to: Point,
    /// 线宽。
    pub width: f64,
    /// 是否虚线（芳香键的内侧线）。
    pub dashed: bool,
    /// 线色；`None` 表示渲染器的默认墨色。
    pub color: Option<Color>,
}

impl LinePlan {
    /// 创建一条实线。
    pub fn new(from: Point, to: Point, width: f64) -> Self {
        Self { from, to, width, dashed: false, color: None }
    }

    /// 改为虚线。
    pub fn dashed(mut self) -> Self {
        self.dashed = true;
        self
    }

    /// 指定线色。
    pub fn with_color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }
}

/// 一个文本标签（原子符号、加号、条件）。
#[derive(Clone, Debug, PartialEq)]
pub struct LabelPlan {
    /// 文本内容。
    pub text: String,
    /// 基线锚点。
    pub at: Point,
    /// 字号。
    pub size: f64,
    /// 水平对齐。
    pub anchor: TextAnchor,
    /// 是否需要在文字下方垫一块背景遮住穿过它的键。
    pub backdrop: bool,
    /// 文字颜色；`None` 表示渲染器的默认墨色。
    pub color: Option<Color>,
}

/// 一个反应箭头。
#[derive(Clone, Debug, PartialEq)]
pub struct ArrowPlan {
    /// 箭尾。
    pub from: Point,
    /// 箭头端点。
    pub to: Point,
    /// 箭头类型。
    pub kind: ArrowKind,
    /// 箭头上方条件。
    pub above: Option<LabelPlan>,
    /// 箭头下方条件。
    pub below: Option<LabelPlan>,
}

/// 完整描绘计划。
#[derive(Clone, Debug, PartialEq)]
pub struct DepictionPlan {
    /// 画布尺寸。
    pub canvas: Size,
    /// 键与箭头杆。
    pub lines: Vec<LinePlan>,
    /// 文本标签。
    pub labels: Vec<LabelPlan>,
    /// 反应箭头（含条件标签）。
    pub arrows: Vec<ArrowPlan>,
    /// 编译期非致命诊断。
    pub diagnostics: Vec<Diagnostic>,
}
