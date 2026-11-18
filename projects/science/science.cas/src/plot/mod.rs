//! 1D 函数采样与 plotter 渲染组装（宿主侧绘图输入）。

mod render;
mod sample;
mod types;

pub use render::{PlotRenderError, line_plot_from_curve, plot_1d_svg, render_line_svg, sampled_curve_to_table};
pub use sample::sample_1d;
pub use types::{SampleDomain, SamplePoint, SampledCurve, SamplingPolicy};
