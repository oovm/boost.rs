//! 确定性排版结果。
//!
//! [`EngravingPlan`] 只携带几何与文本意图（画布尺寸、绝对坐标、字形码位、
//! 诊断），不携带 SVG 字符串、绘制命令或渲染指令。`render` 是唯一消费者。

use plotter_scene::{PathData, TextAnchor, TextStyle};
use plotter_types::{Diagnostic, Point, Size};

/// 排版调参。
#[derive(Clone, Debug, PartialEq)]
pub struct EngravingOptions {
    /// 一个谱表间距的场景单位。
    pub staff_space: f64,
    /// 单行谱表系统的最大宽度（含页边）。
    pub max_line_width: f64,
}

impl Default for EngravingOptions {
    fn default() -> Self {
        Self { staff_space: 10.0, max_line_width: 640.0 }
    }
}

/// 一个 SMuFL 字形放置：码位、基线锚点与字号。
#[derive(Clone, Debug, PartialEq)]
pub struct GlyphPlan {
    /// SMuFL 码位。
    pub glyph: char,
    /// 基线锚点（画布绝对坐标）。
    pub at: Point,
    /// 文本计划样式，`font_family` 固定为 Bravura。
    pub style: TextStyle,
    /// 稳定标识（如 `note-3-2`），供场景 id 与下游命中测试。
    pub id: Option<String>,
}

impl GlyphPlan {
    /// 在谱表间距为 `space` 时创建一个 Bravura 字形。
    pub fn bravura(glyph: char, at: Point, space: f64) -> Self {
        Self {
            glyph,
            at,
            style: TextStyle {
                fill: Some(plotter_types::Color::BLACK),
                opacity: 1.0,
                font_family: Some("Bravura".to_string()),
                font_size: 4.0 * space,
                font_weight: None,
                text_anchor: TextAnchor::Start,
            },
            id: None,
        }
    }

    /// 附加稳定标识。
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }
}

/// 一条直线段（谱表线、符干、加线、小节线）。
#[derive(Clone, Debug, PartialEq)]
pub struct LinePlan {
    /// 起点。
    pub from: Point,
    /// 终点。
    pub to: Point,
    /// 线宽（场景单位）。
    pub width: f64,
    /// 稳定标识。
    pub id: Option<String>,
}

impl LinePlan {
    /// 创建一条线。
    pub fn new(from: Point, to: Point, width: f64) -> Self {
        Self { from, to, width, id: None }
    }

    /// 附加稳定标识。
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }
}

/// 一个圆点（反复记号的点）。
#[derive(Clone, Debug, PartialEq)]
pub struct DotPlan {
    /// 圆心。
    pub center: Point,
    /// 半径。
    pub radius: f64,
}

/// 一条弧线（连音线 / 圆滑线 / 连音组括号）。
#[derive(Clone, Debug, PartialEq)]
pub struct ArcPlan {
    /// 二次贝塞尔路径。
    pub path: PathData,
    /// 线宽。
    pub width: f64,
}

/// 一条歌词或正文文本。
#[derive(Clone, Debug, PartialEq)]
pub struct TextPlan {
    /// 文本内容。
    pub text: String,
    /// 基线锚点。
    pub at: Point,
    /// 文本样式（宿主正文字体）。
    pub style: TextStyle,
}

/// 一个音符事件的全部几何。
#[derive(Clone, Debug, PartialEq)]
pub struct NotePlan {
    /// 符头字形（和弦为每个音高一个）。
    pub heads: Vec<GlyphPlan>,
    /// 临时记号字形。
    pub accidentals: Vec<GlyphPlan>,
    /// 符干（全音符/二分音符无符干）。
    pub stem: Option<LinePlan>,
    /// 符尾（被符杠接管时无）。
    pub flags: Vec<GlyphPlan>,
    /// 加线。
    pub ledger_lines: Vec<LinePlan>,
    /// 发音记号（断奏圆点用 `dots`，保持线用 `lines`）。
    pub articulation_dots: Vec<DotPlan>,
    /// 发音记号线段。
    pub articulation_lines: Vec<LinePlan>,
    /// 歌词。
    pub lyric: Option<TextPlan>,
    /// 事件中心 x。
    pub x: f64,
}

/// 一条符杠（含十六分双杠时 `bands` 为两条）。
#[derive(Clone, Debug, PartialEq)]
pub struct BeamPlan {
    /// 符杠条带（平行四边形顶点，画布绝对坐标）。
    pub bands: Vec<[Point; 4]>,
}

/// 一个小节线计划。
#[derive(Clone, Debug, PartialEq)]
pub struct BarlinePlan {
    /// x 位置。
    pub x: f64,
    /// 类型（决定线组合与点）。
    pub kind: crate::spec::Barline,
}

/// 一行谱表系统。
#[derive(Clone, Debug, PartialEq)]
pub struct SystemPlan {
    /// 谱表左上角（第一线上沿）。
    pub origin: Point,
    /// 谱表宽度（不含页边）。
    pub width: f64,
    /// 谱表间距。
    pub space: f64,
    /// 谱号 / 调号 / 拍号字形。
    pub header_glyphs: Vec<GlyphPlan>,
    /// 全部音符事件几何，按时间顺序。
    pub notes: Vec<NotePlan>,
    /// 符杠。
    pub beams: Vec<BeamPlan>,
    /// 弧线（tie / slur / tuplet bracket）。
    pub arcs: Vec<ArcPlan>,
    /// 小节线。
    pub barlines: Vec<BarlinePlan>,
    /// 反复点。
    pub repeat_dots: Vec<DotPlan>,
    /// 连音组数字。
    pub tuplet_numbers: Vec<TextPlan>,
    /// 速度文本。
    pub tempo: Option<TextPlan>,
    /// 速度音符字形。
    pub tempo_glyph: Option<GlyphPlan>,
}

impl SystemPlan {
    /// 五条谱表线的计划（由 origin 与 space 推导，渲染时发出）。
    pub fn staff_lines(&self) -> Vec<LinePlan> {
        (0..5)
            .map(|line| {
                let y = self.origin.y + line as f64 * self.space;
                LinePlan::new(Point::new(self.origin.x, y), Point::new(self.origin.x + self.width, y), 0.12 * self.space)
            })
            .collect()
    }
}

/// 完整排版计划。
#[derive(Clone, Debug, PartialEq)]
pub struct EngravingPlan {
    /// 画布尺寸。
    pub canvas: Size,
    /// 谱表间距。
    pub space: f64,
    /// 行系统，自上而下。
    pub systems: Vec<SystemPlan>,
    /// 标题。
    pub title: Option<TextPlan>,
    /// 作者。
    pub composer: Option<TextPlan>,
    /// 编译期非致命诊断。
    pub diagnostics: Vec<Diagnostic>,
}
