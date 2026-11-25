//! Semantic MIR -> Wasm executable prepare/emit (**wasm-gc** required for references).
//!
//! **Not** a second MIR. Language CFG/types stay in Semantic MIR; this facade and its
//! submodules own Wasm physical encoding only.
//!
//! 本子系统只消费 [`super::input`] 结算出的 Wasm 私有词汇：跨目标公共 IR
//! （`FragmentSubmission` / `TargetExecutionPlan` / 公共可执行合同与语义类型）
//! 在 `settle_from_submission` 处收敛，编码层不再直接读取任何公共形状。
//!
//! - [control] — CFG block dispatch helpers
//! - [representation] — unique Wasm storage / value-type rules
//! - [type_registry] — GC type-index registration
//! - [calls] — resolved calls -> Wasm call forms
//! - [imports] — host import collection, validation, index-building
//! - [strings] — WASI string-literal data section + function-type byte helpers
//! - [lowerer] — per-function WasmMirLowerer struct + full impl
//! - [entry] — two pub(crate) entrances that drive module assembly
//! - **Value aggregates** (structure / tuple / [T; N]): linear memory
//! - **Reference aggregates** (class / heap [T]): wasm-gc struct/array ops

// ── Local re-exports (子模块之间共用) ──────────────────────────────────────

// 从 data_binary_wasm re-export（calls.rs / control.rs / lowerer.rs 通过 super::* 消费）
pub(super) use data_binary_wasm::{BLOCKTYPE_EMPTY, VALTYPE_ANYREF, VALTYPE_F64, VALTYPE_I32, VALTYPE_I64, WasmOpcode};

/// `f32` 在 wasm 侧的唯一 valtype 常量（`0x7D`）；权威定义见 `wasm::gc::VALTYPE_F32`。
pub(super) use crate::lowering::backends::wasm::gc::VALTYPE_F32;

/// `f32` 的常量与 memarg 编码由 `data-binary-wasm` 拥有；emitter 不再保留本地发射点。
pub(super) use data_binary_wasm::{encode_f32_const, encode_f32_load};

// ── Wasm 私有结算词汇（唯一来源：super::input）────────────────────────────
pub(super) use super::input::{
    abi_structure_for_type, abi_structure_for_type_with_layouts, canonical_parameter_identities, canonical_type_spelling, collect_reachable_blocks,
    declared_text_projection, operand_carrier, primary_result, sum_identity_facts, sum_variants_of, unresolved_text_abi_reason,
    validate_frozen_operator, wasm_export_abi, wasm_gc_field_type_byte, wasm_gc_value_carrier_valtype, wasm_js_glue_value_carrier_valtype,
    WasmAbiField, WasmAbiStructure, WasmAbiVariant, WasmAggregateStructure, WasmArrayInit, WasmBlock, WasmBlockKey, WasmCallableKey, WasmConstant,
    WasmDispatchKind, WasmFieldFact, WasmFieldKey, WasmFloat64Bits, WasmFunctionInput, WasmFunctionType, WasmImportFact, WasmImportLinkFact,
    WasmInstruction, WasmInstructionKey, WasmLayoutFact, WasmLayoutKey, WasmLayoutKeyFact, WasmModuleInput, WasmNominalKey, WasmOperation,
    WasmOperand, WasmProgramTextRepresentation, WasmReceiverPassingKind, WasmSequenceLevel, WasmSequenceStructure, WasmSingletonFact,
    WasmStorageClass, WasmSumLayoutFact, WasmSumRep, WasmSumVariantFact, WasmSumVariantLayoutFact, WasmTerminator, WasmTypeFact, WasmValue,
    WasmValueKey, WasmValueOrigin, WasmVariantKey, WasmWitnessObject,
};

// GC 数组冻结计划是 wasm lane 私有事实，定义在本 lane 的 `array` 模块。
pub(crate) use array::{WasmGcArrayOperation, WasmGcArrayOperatorPlan};

pub(crate) use operator::{WasmOperatorBinding, freeze_wasm_operator_binding};

// 从 wasm gc 模块 re-export
pub(super) use super::gc::WASM_GC_ANYREF;

// 从子模块 re-export（供兄弟子模块通过 super::* 消费）
pub(crate) use lir::{WasmBodyInstruction, encode_wasm_body};
pub(super) use lowerer::{
    HostHandleToManagedUtf8Plan, ManagedUtf8ToHostHandlePlan, Utf8HostHandleMaterializePlan, WASM_GC_EXTERNREF, WasmMirLowerer,
    array_element_type,
};
pub(super) use representation::*;
pub(super) use type_registry::*;

// ── Submodules ────────────────────────────────────────────────────────────

mod abi_schema;
pub(crate) mod array;
mod calls;
mod control;
mod encode;
mod entry;
pub(crate) mod imports;
mod instructions;
mod intrinsics;
mod library_invoke;
mod lir;
mod lowerer;
pub(crate) mod numeric;
mod operands;
mod operator;
mod representation;
pub(crate) mod singleton;
mod strings;
mod type_registry;

// ── Facade pub(crate) entrances (wasm 后端唯一对外入口) ──────────────────

pub(crate) use entry::{augment_wasm_with_value_aggregate_metadata, lower_fragment_mir_to_wasm_module_for};
