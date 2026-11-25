use data_binary_odin::{Odin_VERSION, OdinModuleData};

use super::sanitize_symbol;
use crate::FragmentSubmission;

/// OVM 目标私有 LIR 与 `.odin` 函数体编码器。
pub(crate) mod lir;

/// OVM 目标私有输入：已编码的 `.odin` 模块与库导出 ABI 合同。
///
/// 计划事实（实例门、函数体编码、布局/sum 登记、导出面派生）在
/// [`build_ovm_program_input`] 里一次结算；装配服务 [`assemble_ovm_module`]
/// 只接受这个形状，不再接触跨目标的 `FragmentSubmission` / `ExecutableFunction` /
/// `InstructionKind` / `Terminator`。
pub(crate) struct OvmProgramInput {
    module: OdinModuleData,
    abi_contract: Option<crate::OvmAbiContract>,
}

impl OvmProgramInput {
    /// 已编码的 `.odin` 模块。
    pub(crate) fn module(&self) -> &OdinModuleData {
        &self.module
    }

    /// 库导出 ABI 合同。
    pub(crate) fn abi_contract(&self) -> Option<&crate::OvmAbiContract> {
        self.abi_contract.as_ref()
    }
}

/// 只发射 Compiler 实例键控的目标私有计划，不重放调用边（集成测试专用）。
#[doc(hidden)]
pub fn lower_fragment_to_ovm_module(submission: &FragmentSubmission) -> crate::Result<OdinModuleData> {
    assemble_ovm_module(&build_ovm_program_input(submission)?).map(|(module, _)| module)
}

/// 计划事实 → OVM 目标私有输入（不做模块自检、不写 `.odin` 容器）。
pub(crate) fn build_ovm_program_input(submission: &FragmentSubmission) -> crate::Result<OvmProgramInput> {
    if submission.backend_plan.instances().is_empty() {
        return Err(format!("Nyar VM backend requires Compiler-owned executable functions; edge-based semantic replay is not a valid input"));
    }
    let (module, abi_contract) = super::ovm_mir::lower_fragment_mir_to_ovm_module_with_abi(submission)?;
    Ok(OvmProgramInput { module, abi_contract })
}

/// OVM 目标私有输入 → 模块产物：只做模块形状自检。
///
/// 签名里没有跨目标共享程序：`.odin` 容器编码由 `data-binary-odin` 在同一份
/// 目标私有输入上完成。
pub(crate) fn assemble_ovm_module(input: &OvmProgramInput) -> crate::Result<(OdinModuleData, Option<crate::OvmAbiContract>)> {
    let module = input.module();
    if module.version == 0 || module.functions.is_empty() {
        return Err(format!("Nyar VM 装配：目标私有输入不是有效模块（版本为 0 或没有函数）"));
    }
    Ok((module.clone(), input.abi_contract().cloned()))
}

/// 发射模块并派生 OVM 库导出 ABI 合同（生产路径）。
pub(crate) fn lower_fragment_to_ovm_module_with_abi(
    submission: &FragmentSubmission,
) -> crate::Result<(OdinModuleData, Option<crate::OvmAbiContract>)> {
    assemble_ovm_module(&build_ovm_program_input(submission)?)
}

/// 集成测试入口：与生产路径相同，派生 OVM 库导出 ABI 合同。
#[doc(hidden)]
pub fn lower_fragment_to_ovm_module_with_abi_for_testing(
    submission: &FragmentSubmission,
) -> crate::Result<(OdinModuleData, Option<crate::OvmAbiContract>)> {
    lower_fragment_to_ovm_module_with_abi(submission)
}

/// 库模式公开导出名（run-contract 入口）；与模块内 `NyarExport.symbol_name` 一一对应（集成测试专用）。
#[doc(hidden)]
pub fn library_public_export_names(submission: &FragmentSubmission) -> Vec<String> {
    super::ovm_mir::nyar_library_public_export_names(submission)
}
