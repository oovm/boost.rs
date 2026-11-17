#![deny(missing_docs)]
//! 化学结构描绘链：`MoleculeSpec / ReactionSpec` → `DepictionPlan` → 语义场景 → 显示场景。
//!
//! 与 `plotter.diagram` / `plotter.chart` / `plotter.engraving` 平行的矢量链，只共享
//! `plotter.types` + `plotter.scene`。三层合同：
//!
//! 1. [`spec::MoleculeSpec`] / [`spec::ReactionSpec`] 描述分子图与反应式**是什么**。
//!    语言前端（`plotter.input.smiles` / `plotter.input.mdl`）降级进这个模型。
//! 2. [`plan::DepictionPlan`] 是确定性平面描绘结果：画布、键线、原子标签、箭头与诊断。
//! 3. [`plotter_scene::SemanticScene`] 由 [`render::render`] 产出，再由
//!    [`plotter_scene::SceneResolver`] 解析成 [`plotter_scene::DisplayScene`]。
//!
//! 失败模型与相邻链一致：致命失败返回 [`DepictionError`]；非致命损失写进
//! [`plan::DepictionPlan::diagnostics`] 并复制到 [`LoweredDepiction::diagnostics`]。

/// 稳定的化学描绘诊断码。
///
/// 常量只在 `plotter_types::codes` 定义一次；本模块是消费者唯一的导入路径。
pub mod codes {
    pub use plotter_types::codes::*;
}

pub mod depict;
pub mod error;
pub mod plan;
pub mod render;
pub mod spec;

pub use depict::{depict_molecule, depict_reaction};
pub use error::DepictionError;
pub use plan::{DepictionOptions, DepictionPlan};
pub use render::render;

use plotter_scene::DisplayScene;
use plotter_types::Diagnostic;

use crate::spec::{MoleculeSpec, ReactionSpec};

/// 一次完整降级的结果：描绘计划、解析后的显示场景与诊断。
#[derive(Clone, Debug, PartialEq)]
pub struct LoweredDepiction {
    /// 确定性描绘结果。
    pub plan: DepictionPlan,
    /// 由 [`LoweredDepiction::plan`] 解析出的显示场景。
    pub scene: DisplayScene,
    /// 编译期的非致命损失记录。
    pub diagnostics: Vec<Diagnostic>,
}

/// 把一个分子描绘并解析成显示场景。
pub fn molecule_to_scene(spec: &MoleculeSpec) -> Result<LoweredDepiction, DepictionError> {
    molecule_to_scene_with_options(spec, &DepictionOptions::default())
}

/// 带描绘调参的 [`molecule_to_scene`]。
pub fn molecule_to_scene_with_options(spec: &MoleculeSpec, options: &DepictionOptions) -> Result<LoweredDepiction, DepictionError> {
    let plan = depict_molecule(spec, options)?;
    Ok(finish(plan))
}

/// 把一个反应式描绘并解析成显示场景。
pub fn reaction_to_scene(spec: &ReactionSpec) -> Result<LoweredDepiction, DepictionError> {
    reaction_to_scene_with_options(spec, &DepictionOptions::default())
}

/// 带描绘调参的 [`reaction_to_scene`]。
pub fn reaction_to_scene_with_options(spec: &ReactionSpec, options: &DepictionOptions) -> Result<LoweredDepiction, DepictionError> {
    let plan = depict_reaction(spec, options)?;
    Ok(finish(plan))
}

fn finish(plan: DepictionPlan) -> LoweredDepiction {
    let semantic = render::render(&plan);
    let scene = semantic.resolve(&plotter_scene::SceneResolver::default());
    LoweredDepiction { diagnostics: plan.diagnostics.clone(), plan, scene }
}
