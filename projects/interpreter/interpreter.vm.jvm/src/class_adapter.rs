//! `acorn-jvm` 解码指令到执行子集的适配器。
//!
//! class 文件解析、常量池与属性表由格式层拥有；本模块只把已解码的
//! [`acorn_jvm::class::JvmInstruction`] 映射为可执行子集，超出子集时显式拒绝，
//! 不做静默丢弃或近似翻译。

use acorn_jvm::class::JvmInstruction;

use crate::{error::JvmError, op::Instruction};

/// 把一条已解码指令映射为执行子集指令。
///
/// 标签、引用类型与未实现的操作返回 [`JvmError::UnsupportedInstruction`]。
pub fn from_class_instruction(instruction: &JvmInstruction) -> Result<Instruction, JvmError> {
    let mapped = match instruction {
        JvmInstruction::IConst(value) => Instruction::IConst(*value),
        JvmInstruction::LConst0 => Instruction::LConst(0),
        JvmInstruction::LConst1 => Instruction::LConst(1),
        JvmInstruction::ILoad(index) => Instruction::ILoad(usize::from(*index)),
        JvmInstruction::LLoad(index) => Instruction::LLoad(usize::from(*index)),
        JvmInstruction::IStore(index) => Instruction::IStore(usize::from(*index)),
        JvmInstruction::LStore(index) => Instruction::LStore(usize::from(*index)),
        JvmInstruction::Pop => Instruction::Pop,
        JvmInstruction::IAdd => Instruction::IAdd,
        JvmInstruction::ISub => Instruction::ISub,
        JvmInstruction::IMul => Instruction::IMul,
        JvmInstruction::IDiv => Instruction::IDiv,
        JvmInstruction::LAdd => Instruction::LAdd,
        JvmInstruction::I2L => Instruction::I2L,
        JvmInstruction::IReturn => Instruction::IReturn,
        JvmInstruction::LReturn => Instruction::LReturn,
        JvmInstruction::Return => Instruction::Return,
        other => return Err(JvmError::UnsupportedInstruction { instruction: format!("{other:?}") }),
    };
    Ok(mapped)
}

/// 批量映射一段已解码指令。
pub fn from_class_program(program: &[JvmInstruction]) -> Result<Vec<Instruction>, JvmError> {
    program.iter().map(from_class_instruction).collect()
}
