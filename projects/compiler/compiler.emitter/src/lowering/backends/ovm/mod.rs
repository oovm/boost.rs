//! OVM（Odin VM）目标私有编码服务。
//!
//! OVM lane 已切到语言侧直出：MIR → 目标私有输入的结算发生在
//! `language.valkyrie` 的 `pipeline/ovm/`；本模块只保留 OVM 目标私有的编码原语
//! （LIR/字节码发射器、operator 绑定）与模块形状自检，ABI 合同由语言 × OVM
//! 适配器结算、本 crate 只做目标侧校验，
//! 不再读取跨目标共享程序载荷——所有计划事实由语言侧结算后，
//! 以目标私有输入（`OvmProgramInput`）一次传入。

use data_binary_odin::{OdinModuleData, Odin_VERSION};

/// OVM 目标私有的 operator 绑定与整数载体事实。
pub mod operator;

/// OVM 目标私有 LIR、字节码发射器与 `.odin` 函数体编码器。
pub mod lir;

/// OVM 目标私有输入：已编码的 `.odin` 模块与库导出 ABI 合同。
///
/// 计划事实（实例门、函数体编码、布局/sum 登记、导出面派生）由语言侧 OVM lane
/// 一次结算；装配服务 [`assemble_ovm_module`] 只接受这个形状。
pub struct OvmProgramInput {
    module: OdinModuleData,
    abi_contract: Option<data_binary_odin::OvmAbiContract>,
}

impl OvmProgramInput {
    /// 从语言侧结算好的模块与 ABI 合同构造目标私有输入。
    pub fn new(module: OdinModuleData, abi_contract: Option<data_binary_odin::OvmAbiContract>) -> Self {
        Self { module, abi_contract }
    }

    /// 已编码的 `.odin` 模块。
    pub fn module(&self) -> &OdinModuleData {
        &self.module
    }

    /// 库导出 ABI 合同。
    pub fn abi_contract(&self) -> Option<&data_binary_odin::OvmAbiContract> {
        self.abi_contract.as_ref()
    }
}

/// OVM 目标私有输入 → 模块产物：只做模块形状自检。
///
/// 签名里没有跨目标共享程序：`.odin` 容器编码由 `data-binary-odin` 在同一份
/// 目标私有输入上完成。
pub fn assemble_ovm_module(input: &OvmProgramInput) -> crate::Result<(OdinModuleData, Option<data_binary_odin::OvmAbiContract>)> {
    let module = input.module();
    if module.version == 0 || module.functions.is_empty() {
        return Err(format!("Nyar VM 装配：目标私有输入不是有效模块（版本为 0 或没有函数）"));
    }
    Ok((module.clone(), input.abi_contract().cloned()))
}
