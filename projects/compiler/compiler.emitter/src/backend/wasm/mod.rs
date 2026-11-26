//! Wasm 目标私有装配层对外接口。
//!
//! 该模块是 wasm lane 纵链第二段的唯一入口面：语言侧用这里的私有词汇结算
//! 目标输入（[`WasmProgramInput`]），再交 [`assemble_wasm_module`] 完成段编码、
//! 自定义段装配与模块形状自检。跨目标公共 IR 不出现在任何签名里。

pub use crate::lowering::backends::wasm::abi::{WasmAbiCarrier, WasmLibraryExportAbi};
pub use crate::lowering::backends::wasm::input::{
    WasmAbiField, WasmAbiStructure, WasmAbiVariant, WasmAggregateStructure, WasmArrayInit, WasmBlock, WasmBlockKey, WasmCallableKey, WasmConstant,
    WasmDispatchKind, WasmFieldFact, WasmFieldKey, WasmFloat64Bits, WasmFrozenExportAbi, WasmFunctionInput, WasmFunctionType, WasmHostImportProviderFact,
    WasmImportFact, WasmImportLinkFact, WasmInstruction, WasmInstructionKey, WasmLayoutFact, WasmLayoutKey, WasmLayoutKeyFact, WasmModuleInput,
    WasmNominalKey, WasmOperation, WasmOperand, WasmProgramTextRepresentation, WasmReceiverPassingKind, WasmSequenceLevel, WasmSequenceStructure,
    WasmSingletonFact, WasmStorageClass, WasmSumLayoutFact, WasmSumRep, WasmSumVariantFact, WasmSumVariantLayoutFact, WasmTerminator, WasmTypeFact,
    WasmValue, WasmValueKey, WasmValueOrigin, WasmVariantKey, abi_structure_for_type, canonical_parameter_identities, declared_text_projection,
    sum_identity_facts, unresolved_text_abi_reason, wasm_export_abi, wasm_host_provider_carrier_valtype,
};
pub use crate::lowering::backends::wasm::mir::abi_schema::{WasmAbiType, abi_schema_for_type};
pub use crate::lowering::backends::wasm::{WasmProgramInput, assemble_wasm_module, host_entry_export_name};
pub use crate::lowering::features::physical_contract::{PhysicalTextBoundary, PhysicalTextCarrier, UTF16_UNRESOLVED_TEXT_ABI};
pub use crate::lowering::shared::physical_layout::{LayoutPolicy, ResolvedAggregateLayout, ResolvedFieldLayout, ResolvedLayoutPlan, resolve_physical_layouts};
