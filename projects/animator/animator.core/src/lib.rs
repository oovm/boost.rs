#![deny(missing_docs)]
//! 图表与示意图的动画程序。
//!
//! [`plotter_motion`] 回答的是“两个静态场景之间的一段过渡怎么插值”，本身不拥有
//! 整条时间轴。真实的科普图表需要更长的结构：坐标轴先出现、数据柱逐段长高、
//! 再切到下一份数据。本 crate 把状态序列与每段过渡编译成时间轴：
//!
//! ```text
//! 状态序列 + 每段过渡参数 -> Program -> compile -> Compiled::sample(t) -> DisplayScene
//! ```
//!
//! 它不加载数据、不排版、不渲染、也不读时钟；宿主给出秒数或帧号，输出场景。
//! 动画通道完全继承 [`plotter_motion`]：只有 `Transform` 与 `Opacity`。
//!
//! # 时间语义
//!
//! - 状态按 `at` 严格升序排列，第一个状态之前的取样一律回到第一个状态。
//! - 第 `i` 段过渡从 `states[i].at` 开始，最多持续 `transitions[i].duration` 秒；
//!   过渡结束后到下一个状态开始之前保持下一个状态的画面。
//! - `duration == 0` 是显式硬切，不是“很短的一段过渡”。

use std::collections::BTreeSet;
use std::fmt;

use plotter_motion::{EnterPolicy, ExitPolicy, InterruptionPolicy, MotionIntent, TransitionPlan, plan_transition, sample};
use plotter_scene::DisplayScene;

pub use plotter_motion::{Channel, Easing};

/// 时间轴上的一个静态状态。
#[derive(Clone, Debug, PartialEq)]
pub struct SceneState {
    /// 该状态开始生效的秒数。
    pub at: f64,
    /// 已经排好版的静态场景。
    pub scene: DisplayScene,
}

/// 连接相邻两个状态的一段过渡。
///
/// `duration` 超过两状态间隔属于显式错误，避免“动画还没播完就被下一个状态覆盖”
/// 这种难以察觉的时间轴缺口。
#[derive(Clone, Debug, PartialEq)]
pub struct Transition {
    /// 过渡时长（秒），`0` 表示直接切换。
    pub duration: f64,
    /// 归一化进度上的缓动。
    pub ease: Easing,
    /// 对两个状态中都存在的节点执行插值的通道。
    ///
    /// 缺省包含 `Transform`、`Extent` 与 `Opacity`：前两者让图表柱从共享基线
    /// 长高，后者负责淡入淡出。`Extent` 只作用于 `Rect` / `RoundedRect` 节点，
    /// 其它节点不会凭空获得几何通道。
    pub channels: BTreeSet<Channel>,
    /// 只存在于后一个状态的节点如何出现。
    pub enter: EnterPolicy,
    /// 只存在于前一个状态的节点如何消失。
    pub exit: ExitPolicy,
}

impl Default for Transition {
    /// 一个可用缺省：0.4 秒、缓入缓出、变换/矩形/透明度三个通道、进出都淡入淡出。
    fn default() -> Self {
        Self {
            duration: 0.4,
            ease: Easing::EaseInOut,
            channels: BTreeSet::from([Channel::Transform, Channel::Extent, Channel::Opacity]),
            enter: EnterPolicy::Fade,
            exit: ExitPolicy::Fade,
        }
    }
}

/// 一份动画程序：状态序列与逐段过渡。
#[derive(Clone, Debug, PartialEq)]
pub struct Program {
    /// 按时间升序排列的状态。
    pub states: Vec<SceneState>,
    /// 长度必须为 `states.len() - 1`，第 `i` 段连接状态 `i` 与 `i + 1`。
    pub transitions: Vec<Transition>,
}

/// 程序无法编译时给出的定位化错误。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ProgramError {
    /// 没有任何状态，无法定义时间轴。
    NoStates,
    /// 过渡数量与状态数量不匹配。
    TransitionCount {
        /// 状态数量。
        states: usize,
        /// 过渡数量。
        transitions: usize,
    },
    /// 第 `index` 个状态的时间不是有限数。
    TimeNotFinite {
        /// 出问题的状态下标。
        index: usize,
    },
    /// 第 `index` 个状态的时间没有严格晚于前一个状态。
    TimeNotAscending {
        /// 出问题的状态下标。
        index: usize,
    },
    /// 第 `index` 段过渡的时长是负数。
    NegativeDuration {
        /// 出问题的过渡下标。
        index: usize,
    },
    /// 第 `index` 段过渡比它覆盖的间隔更长。
    TransitionTooLong {
        /// 出问题的过渡下标。
        index: usize,
        /// 声明的过渡时长。
        duration: f64,
        /// 两个状态之间的间隔。
        gap: f64,
    },
}

impl fmt::Display for ProgramError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoStates => write!(formatter, "动画程序至少需要一个状态"),
            Self::TransitionCount { states, transitions } => {
                write!(formatter, "过渡数量必须是状态数量减一：{states} 个状态却给了 {transitions} 段过渡")
            }
            Self::TimeNotFinite { index } => write!(formatter, "第 {index} 个状态的时间不是有限数"),
            Self::TimeNotAscending { index } => write!(formatter, "第 {index} 个状态的时间没有严格晚于前一个状态"),
            Self::NegativeDuration { index } => write!(formatter, "第 {index} 段过渡的时长是负数"),
            Self::TransitionTooLong { index, duration, gap } => {
                write!(formatter, "第 {index} 段过渡时长 {duration} 秒超过它覆盖的 {gap} 秒间隔")
            }
        }
    }
}

impl std::error::Error for ProgramError {}

/// 编译后的时间轴，可以按任意时间重复取样。
#[derive(Clone, Debug)]
pub struct Compiled {
    states: Vec<SceneState>,
    segments: Vec<Segment>,
    duration: f64,
}

#[derive(Clone, Debug)]
struct Segment {
    plan: TransitionPlan,
    duration: f64,
}

impl Program {
    /// 构造一份只有状态、过渡取缺省的程序。
    pub fn new(states: Vec<SceneState>) -> Self {
        let transitions = vec![Transition::default(); states.len().saturating_sub(1)];
        Self { states, transitions }
    }

    /// 检查时间顺序、过渡数量与每段时长。
    pub fn validate(&self) -> Result<(), ProgramError> {
        if self.states.is_empty() {
            return Err(ProgramError::NoStates);
        }
        if self.transitions.len() + 1 != self.states.len() {
            return Err(ProgramError::TransitionCount { states: self.states.len(), transitions: self.transitions.len() });
        }
        for (index, state) in self.states.iter().enumerate() {
            if !state.at.is_finite() {
                return Err(ProgramError::TimeNotFinite { index });
            }
            if index > 0 && state.at <= self.states[index - 1].at {
                return Err(ProgramError::TimeNotAscending { index });
            }
        }
        for (index, transition) in self.transitions.iter().enumerate() {
            if !transition.duration.is_finite() || transition.duration < 0.0 {
                return Err(ProgramError::NegativeDuration { index });
            }
            let gap = self.states[index + 1].at - self.states[index].at;
            if transition.duration > gap {
                return Err(ProgramError::TransitionTooLong { index, duration: transition.duration, gap });
            }
        }
        Ok(())
    }

    /// 整条程序的结束时间；只有一个状态时就是它的开始时间。
    pub fn duration_seconds(&self) -> f64 {
        self.states.last().map_or(0.0, |state| state.at)
    }

    /// 校验并预编译每段过渡，之后取样不再重复规划。
    pub fn compile(&self) -> Result<Compiled, ProgramError> {
        self.validate()?;
        let mut segments = Vec::with_capacity(self.transitions.len());
        for (index, transition) in self.transitions.iter().enumerate() {
            let intent = MotionIntent {
                duration: transition.duration,
                ease: transition.ease,
                channels: transition.channels.clone(),
                enter: transition.enter,
                exit: transition.exit,
                interruption: InterruptionPolicy::CancelAndRestart,
            };
            let plan = plan_transition(&self.states[index].scene, &self.states[index + 1].scene, &intent);
            segments.push(Segment { plan, duration: transition.duration });
        }
        Ok(Compiled { states: self.states.clone(), segments, duration: self.duration_seconds() })
    }
}

impl Compiled {
    /// 时间轴总长（秒）。
    pub fn duration_seconds(&self) -> f64 {
        self.duration
    }

    /// 在 `seconds` 处取样。
    ///
    /// 时间会被钳制到 `0.0..=duration_seconds()`：早于第一个状态时回到第一个
    /// 状态的画面，晚于最后一个状态时停在最后一个状态。
    pub fn sample(&self, seconds: f64) -> DisplayScene {
        // `compile` 已拒绝空程序，所以这里可以按下标读取。
        let first = &self.states[0];
        if !seconds.is_finite() || seconds <= first.at {
            return first.scene.clone();
        }
        let last = self.states.len() - 1;
        if seconds >= self.states[last].at {
            return self.states[last].scene.clone();
        }
        let index = self.states.partition_point(|state| state.at <= seconds) - 1;
        let segment = &self.segments[index];
        let local = seconds - self.states[index].at;
        if segment.duration <= 0.0 || local >= segment.duration {
            // 过渡已经结束：保持下一个状态，直到时间轴给出新的状态。
            return self.states[index + 1].scene.clone();
        }
        sample(&segment.plan, &self.states[index].scene, &self.states[index + 1].scene, local)
    }

    /// 按帧率取样：`frame / fps` 秒。
    ///
    /// `fps == 0` 无法定义帧时间，返回 `None` 而不是静默按 0 处理。
    pub fn sample_frame(&self, fps: u32, frame: u32) -> Option<DisplayScene> {
        if fps == 0 {
            return None;
        }
        Some(self.sample(f64::from(frame) / f64::from(fps)))
    }

    /// 覆盖整条时间轴所需的整帧数量（向上取整）。
    ///
    /// 宿主按这个数量循环即可保证末帧落在或超过 `duration_seconds()`；
    /// `fps == 0` 同样返回 `None`。取值只由时长与帧率决定，同一程序在不同
    /// 帧率下的帧号 → 秒映射可以逐项复算。
    pub fn frame_count(&self, fps: u32) -> Option<u32> {
        if fps == 0 {
            return None;
        }
        Some((self.duration * f64::from(fps)).ceil() as u32)
    }
}
