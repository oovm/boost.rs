#![warn(missing_docs)]
//! 图表数据动画：把同一张图表的多个数据快照编译成 [`animator_core::Program`]。
//!
//! ```text
//! ChartSpec + 多份 DataSnapshot（各自带时间）
//!   -> 每份快照编译并解析成 DisplayScene
//!   -> animator.core 的状态序列
//!   -> Compiled::sample(t) / sample_frame(fps, frame)
//! ```
//!
//! 这里只做“数据快照 → 场景状态”的编译。过渡、缓动与进入/退出策略仍归
//! [`animator_core`]（也就是 `plotter.motion`），本 crate 不发明第二套插值，也不
//! 触碰几何：图元的稳定 id 由 `plotter.chart` 生成，插入或过滤系列后动画仍按 id
//! 对应，而不是按下标。

use animator_core::{Program, SceneState, Transition};
use plotter_chart::{Channel, ChartError, ChartOptions, ChartSpec, DataSnapshot, Mark, ScaleSpec, lower};
use plotter_scene::SceneResolver;

/// 时间轴上的一个数据状态。
#[derive(Clone, Debug)]
pub struct ChartState {
    /// 该状态开始生效的秒数。
    pub at: f64,
    /// 这一时刻的完整数据快照。
    pub data: DataSnapshot,
}

/// 一张图表的数据动画程序。
#[derive(Clone, Debug)]
pub struct ChartTimeline {
    /// 图表声明；所有状态共用同一份，保证刻度与通道绑定不变。
    pub spec: ChartSpec,
    /// 编译选项（画布、边距、字号、tick 数）。
    pub options: ChartOptions,
    /// 按时间给出的数据状态。
    pub states: Vec<ChartState>,
    /// 逐段过渡；为空时使用 [`Transition::default`]。
    pub transitions: Vec<Transition>,
    /// 是否让所有状态共用一套数值定义域。
    ///
    /// 默认 `false`：每份快照各自推断定义域，纵轴会随数据缩放，柱高可能看不出
    /// 变化（值翻三倍、轴也翻三倍）。设为 `true` 时，所有状态共用由全部快照
    /// 求并集得到的定义域，柱子才会真正长高。只影响连续位置通道，分类轴不变。
    pub shared_domain: bool,
}

/// 无法把数据状态编译成时间轴的原因。
#[derive(Debug)]
pub enum ChartTimelineError {
    /// 没有数据状态，无法定义时间轴。
    NoStates,
    /// 过渡数量与状态数量不匹配。
    TransitionCount {
        /// 状态数量。
        states: usize,
        /// 过渡数量。
        transitions: usize,
    },
    /// 某一份数据快照无法编译成图表。
    Chart(ChartError),
}

impl std::fmt::Display for ChartTimelineError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoStates => write!(formatter, "图表动画至少需要一个数据状态"),
            Self::TransitionCount { states, transitions } => {
                write!(formatter, "过渡数量必须是状态数量减一：{states} 个状态却给了 {transitions} 段过渡")
            }
            Self::Chart(error) => write!(formatter, "图表编译失败：{error}"),
        }
    }
}

impl std::error::Error for ChartTimelineError {}

impl From<ChartError> for ChartTimelineError {
    fn from(error: ChartError) -> Self {
        Self::Chart(error)
    }
}

impl ChartTimeline {
    /// 用默认过渡连接每个状态。
    pub fn new(spec: ChartSpec, options: ChartOptions, states: Vec<ChartState>) -> Self {
        let transitions = vec![Transition::default(); states.len().saturating_sub(1)];
        Self { spec, options, states, transitions, shared_domain: false }
    }

    /// 编译成 `animator.core` 的程序。
    ///
    /// 每份快照都经过 `plotter.chart` 的完整校验；任何一份失败都让整条时间轴失败，
    /// 不会用上一帧的画面顶替。
    pub fn program(&self) -> Result<Program, ChartTimelineError> {
        if self.states.is_empty() {
            return Err(ChartTimelineError::NoStates);
        }
        if !self.transitions.is_empty() && self.transitions.len() + 1 != self.states.len() {
            return Err(ChartTimelineError::TransitionCount { states: self.states.len(), transitions: self.transitions.len() });
        }
        let resolver = SceneResolver::new();
        let spec = if self.shared_domain { self.shared_domain_spec() } else { self.spec.clone() };
        let mut states = Vec::with_capacity(self.states.len());
        for state in &self.states {
            let lowered = lower(&spec, &state.data, &self.options)?;
            states.push(SceneState { at: state.at, scene: resolver.resolve(&lowered.scene) });
        }
        let transitions =
            if self.transitions.is_empty() { vec![Transition::default(); states.len().saturating_sub(1)] } else { self.transitions.clone() };
        Ok(Program { states, transitions })
    }

    /// 由全部快照求并集，给连续位置通道写入显式定义域。
    ///
    /// 柱与面积沿用图表默认的“包含零”规则；分类通道、非有限值和缺失列都跳过。
    /// 空集或单点时给出一个不会退化的域，避免除零。
    fn shared_domain_spec(&self) -> ChartSpec {
        let mut spec = self.spec.clone();
        for channel in [Channel::X, Channel::Y] {
            let Some(encoding) = spec.encoding.channel(channel)
            else {
                continue;
            };
            let name = encoding.field.name.clone();
            if !encoding.field.kind.is_quantitative() {
                continue;
            }
            let (mut min, mut max) = (f64::INFINITY, f64::NEG_INFINITY);
            for state in &self.states {
                let Some(column) = state.data.columns.iter().find(|column| column.name == name)
                else {
                    continue;
                };
                for value in &column.values {
                    let Some(number) = value.as_number()
                    else {
                        continue;
                    };
                    if number.is_finite() {
                        min = min.min(number);
                        max = max.max(number);
                    }
                }
            }
            if !min.is_finite() || !max.is_finite() {
                continue;
            }
            if matches!(spec.mark, Mark::Bar | Mark::Area) {
                min = min.min(0.0);
                max = max.max(0.0);
            }
            if max <= min {
                max = min + 1.0;
            }
            spec.scales.set(channel, ScaleSpec::Linear { domain: Some((min, max)), zero: None, nice: true });
        }
        spec
    }
}
