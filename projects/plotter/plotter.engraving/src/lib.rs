#![warn(missing_docs)]
//! 乐谱 engraving 链：`ScoreSpec` → `EngravingPlan` → 语义场景 → 显示场景。
//!
//! 与 `plotter.diagram` / `plotter.chart` 平行的第三条矢量链，只共享
//! `plotter.types` + `plotter.scene`。三层合同：
//!
//! 1. [`spec::ScoreSpec`] 描述一张谱**是什么**：谱号、调号、拍号、小节与音高事件。
//!    语言前端（`plotter.input.abc` / `plotter.input.musicxml`）降级进这个模型，
//!    从不直接构造场景节点或 SVG。
//! 2. [`plan::EngravingPlan`] 是确定性排版结果：画布尺寸、行系统、绝对坐标、
//!    SMuFL 字形码位与诊断。只携带几何与文本意图。
//! 3. [`plotter_scene::SemanticScene`] 是 [`render::render`] 产出的未解析语义树，
//!    再由 [`plotter_scene::SceneResolver`] 解析成 [`plotter_scene::DisplayScene`]
//!    —— SVG 或其他渲染器的唯一输入。
//!
//! 失败模型与 `plotter.diagram` 一致：致命失败返回 [`EngravingError`] 携带结构化诊断；
//! 非致命损失写进 [`plan::EngravingPlan::diagnostics`] 并复制到
//! [`LoweredEngraving::diagnostics`]，不静默丢弃。本 crate 不依赖 `notedown-ir`，
//! 不定义文档语义，不渲染自身。

/// 稳定的乐谱诊断码。
///
/// 常量只在 `plotter_types::codes` 定义一次；本模块是消费者唯一的导入路径，
/// 避免前端与宿主绑定漂移到两种拼写。
pub mod codes {
    pub use plotter_types::codes::*;
}

pub mod engrave;
pub mod error;
pub mod plan;
pub mod render;
pub mod smufl;
pub mod spec;

pub use engrave::engrave;
pub use error::EngravingError;
pub use plan::{EngravingOptions, EngravingPlan};
pub use render::render;

use plotter_scene::DisplayScene;
use plotter_types::Diagnostic;

use crate::spec::ScoreSpec;

/// 一次完整降级的结果：排版计划、解析后的显示场景与诊断。
#[derive(Clone, Debug, PartialEq)]
pub struct LoweredEngraving {
    /// 确定性排版结果。
    pub plan: EngravingPlan,
    /// 由 [`LoweredEngraving::plan`] 解析出的显示场景。
    pub scene: DisplayScene,
    /// 编译期的非致命损失记录。
    pub diagnostics: Vec<Diagnostic>,
}

/// 把 `ScoreSpec` 直接排版并解析成显示场景。
pub fn score_to_scene(spec: &ScoreSpec) -> Result<LoweredEngraving, EngravingError> {
    score_to_scene_with_options(spec, &EngravingOptions::default())
}

/// 带排版调参的 `score_to_scene`。
pub fn score_to_scene_with_options(spec: &ScoreSpec, options: &EngravingOptions) -> Result<LoweredEngraving, EngravingError> {
    let plan = engrave(spec, options)?;
    let semantic = render(&plan);
    let scene = semantic.resolve(&plotter_scene::SceneResolver::default());
    Ok(LoweredEngraving { plan: plan.clone(), scene, diagnostics: plan.diagnostics.clone() })
}
