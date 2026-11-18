//! science `SampledCurve` → plotter `PlotSpec` / SVG 渲染组装。
//!
//! plotter 不依赖 science；采样在 science，构图与渲染在 plotter。

use plotter::{
    data::ColumnTable,
    grammar::{CompileOptions, LayerSpec, Mapping, PlotSpec, compile_plot},
    renderer::svg::render_svg,
};
use science_types::{Diagnostic, TermId};

use crate::runtime::Session;

use super::{SampleDomain, SampledCurve, SamplingPolicy, sample_1d};

/// 绘图渲染组装错误（结构化 code 优先，文案仅调试）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlotRenderError {
    /// science 采样失败。
    Sampling(Diagnostic),
    /// plotter 数据 / 编译 / 渲染失败（稳定 code）。
    Plotter {
        /// `plotter_*` wire code（上游诊断或组装层同名守卫）。
        code: String,
    },
}

impl PlotRenderError {
    fn plotter(code: impl Into<String>) -> Self {
        Self::Plotter { code: code.into() }
    }
}

/// 将有效采样点转为列式表（gap 点丢弃；跨 gap 连线属后续多段层）。
pub fn sampled_curve_to_table(curve: &SampledCurve) -> Result<ColumnTable, PlotRenderError> {
    let mut xs = Vec::new();
    let mut ys = Vec::new();
    for p in &curve.points {
        if p.valid {
            xs.push(p.x);
            ys.push(p.y);
        }
    }
    if xs.len() < 2 {
        return Err(PlotRenderError::plotter("plotter_empty_data"));
    }
    ColumnTable::new()
        .push_float("x", xs)
        .map_err(|d| PlotRenderError::plotter(d.code.as_str()))?
        .push_float("y", ys)
        .map_err(|d| PlotRenderError::plotter(d.code.as_str()))
}

/// 由采样曲线构造单层折线 `PlotSpec`。
pub fn line_plot_from_curve(curve: &SampledCurve) -> Result<PlotSpec, PlotRenderError> {
    let table = sampled_curve_to_table(curve)?;
    Ok(PlotSpec::new(table).mapping(Mapping::xy("x", "y")).layer(LayerSpec::geom_line()))
}

/// 编译并渲染为 SVG 字符串。
pub fn render_line_svg(curve: &SampledCurve) -> Result<String, PlotRenderError> {
    let plot = line_plot_from_curve(curve)?;
    let scene = compile_plot(&plot, CompileOptions::default()).map_err(|d| PlotRenderError::plotter(d.code.as_str()))?;
    render_svg(&scene).map_err(|d| PlotRenderError::plotter(d.code.as_str()))
}

/// 对一元表达式采样并渲染 SVG（1D plot 垂直切片）。
pub fn plot_1d_svg(
    session: &mut Session,
    expr: TermId,
    var: &str,
    domain: SampleDomain,
    policy: SamplingPolicy,
) -> Result<String, PlotRenderError> {
    let curve = sample_1d(session, expr, var, domain, policy).map_err(PlotRenderError::Sampling)?;
    render_line_svg(&curve)
}
