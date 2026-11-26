//! 单向编译**分析**与**处理**流。
//!
//! ```text
//! Analysis:   AST → HIR → Semantic MIR → Link → Validate (M2) → ValkyrieMirProgram
//! Processing: ValkyrieMirProgram → RepresentationPlan → 语言 × 目标 lane 直出目标私有输入 → Emit
//! ```
//!
//! 本模块只负责 Valkyrie 侧编排。不得复活并行权威
//! （旧前端计划、公共 IR 片段提交体旁路、God Call 字段）。
//! 语义与表示成功载荷归 Valkyrie 自有 MIR；目标输入由语言 × 目标适配器准备。

mod backend_bundle;
mod canonical;
mod context;
mod diagnostics;
mod driver;
mod envelope_checks;
mod error;
mod evidence;
mod host_bindings;
pub(crate) mod lane;
mod link;
pub(crate) mod clr;
pub(crate) mod jvm;
pub(crate) mod machine;
#[cfg(feature = "ovm-lane")]
pub(crate) mod ovm;
pub(crate) mod wasm;
/// 语言 × 目标适配器共享的源语义裁决：声明签名载体判定与冻结计划形状核对。
pub(crate) mod operator;
mod representation;

pub use backend_bundle::{
    CompilerArtifactReport, CompilerBuildBundle, compile_source_groups_to_artifacts, compile_source_groups_to_backend_bundle,
};
pub use canonical::canonical_program_from_semantic_mir;
pub use context::{CompilerBuildContext, CompilerGatedOutHostProvider, CompilerHostProviderBinding};
pub use diagnostics::{diagnostic, fail_stage};
pub use driver::compile_linked_semantic_mir;
pub use envelope_checks::{check_function_envelopes, check_instruction_envelope, expected_result_count};
pub use error::{PipelineError, PipelineResult};
pub use evidence::{CompilerCompileEvidence, generated_source_hash, host_provider_binding_hash_for_bindings, semantic_closure_hash_for_groups};
pub use host_bindings::{
    ResolvedHostProviderBinding, apply_host_provider_bindings, enforce_resolved_host_contract_bindings, host_provider_instantiation_seeds,
    host_provider_root_declarations, prune_unselected_host_provider_external_calls, resolve_host_provider_bindings,
};
pub use lane::{AdapterStage, TargetPlanError};
pub use link::{link_reachable_dependency_mir, link_reachable_dependency_mir_with_hir, merge_aggregate_layouts, merge_entry_group_surface};
pub use representation::CanonicalRepresentationPlanner;

use crate::compiler::valkyrie_mir::pipeline::CompileStage;

/// Which half of the one-way stream a stage belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamHalf {
    /// Produce / close semantic facts (through `ValkyrieMirProgram`).
    Analysis,
    /// Consume canonical facts into layout / private plan / artifacts.
    Processing,
}

/// Map a [`CompileStage`] to analysis vs processing.
pub fn stream_half(stage: CompileStage) -> StreamHalf {
    match stage {
        CompileStage::Ast | CompileStage::Hir | CompileStage::SemanticMir | CompileStage::LinkTime | CompileStage::ValidateMir => {
            StreamHalf::Analysis
        }
        CompileStage::RepresentationPlan | CompileStage::LaneLowering | CompileStage::Emit => StreamHalf::Processing,
    }
}

/// Documented analysis stage order (subset of [`crate::compiler::valkyrie_mir::pipeline::STAGE_ORDER`]).
pub const ANALYSIS_STAGE_ORDER: &[CompileStage] =
    &[CompileStage::Ast, CompileStage::Hir, CompileStage::SemanticMir, CompileStage::LinkTime, CompileStage::ValidateMir];

/// Documented processing stage order.
pub const PROCESSING_STAGE_ORDER: &[CompileStage] = &[CompileStage::RepresentationPlan, CompileStage::LaneLowering, CompileStage::Emit];
