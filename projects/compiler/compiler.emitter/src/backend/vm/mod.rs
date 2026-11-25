//! Emit `.odin` bytecode modules for the Nyar VM backend.
//!
//! 本模块同时是 OVM lane 的**目标私有编码服务**导出面（参照机器码 lane 的
//! `emitter_backend_machine`）：语言侧 `pipeline/ovm/` 直读 MIR 结算计划事实，
//! 经这里导出的字节码发射器、LIR 编码器、operator 绑定与 ABI 合同投影产出
//! `OvmProgramInput`；emitter 不再解释任何跨目标共享程序。

use std::{fs, path::Path};

use crate::{EmitterError, Result};
use data_binary_odin::{OdinModuleData, encode_module};

#[cfg(feature = "ovm-lane")]
pub use crate::lowering::backends::ovm::lir::{
    BytecodeEmitter, OdinLirInstruction, TEXT_FROM_BYTES_IMPORT, TEXT_TO_BYTES_IMPORT, encode_odin_function,
};
#[cfg(feature = "ovm-lane")]
pub use crate::lowering::backends::ovm::operator::{
    I32_NEGATE_STEPS, OvmIntShape, OvmOperatorBinding, OvmOperatorCode, freeze_ovm_operator, freeze_ovm_operator_binding, ovm_host_import_target,
    ovm_int_shape, ovm_operator_target,
};
#[cfg(feature = "ovm-lane")]
pub use crate::lowering::backends::ovm::{OvmProgramInput, assemble_ovm_module};
pub use crate::lowering::shared::interop::select_unique_target;
pub use crate::lowering::shared::physical_layout::{
    LayoutPolicy, ResolvedAggregateLayout, ResolvedFieldLayout, ResolvedLayoutPlan, resolve_physical_layouts,
};
pub use crate::lowering::sanitize_symbol;

/// Encode [`OdinModuleData`] and write it to `output_path`.
pub fn emit_ovm_module(module: &OdinModuleData, output_path: &Path) -> Result<()> {
    let bytes = encode_module(module);

    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| format!("failed to create output directory: {}: {e}", parent.display()))?;
        }
    }

    fs::write(output_path, &bytes).map_err(|e| format!("failed to write odin module: {}: {e}", output_path.display()))?;

    Ok(())
}
