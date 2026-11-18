//! `acorn-wasm` 解码指令到执行子集的适配器。
//!
//! 模块二进制与段解析由格式层拥有；本模块只把已解码的 [`DecodedInstruction`]
//! 映射为可执行子集，超出子集（内存、表、引用类型、块控制流等）时显式拒绝。

use acorn_wasm::instruction::{DecodedInstruction, DecodedOperand};

use crate::{error::WasmError, op::Instruction};

const OP_RETURN: u8 = 0x0F;
const OP_DROP: u8 = 0x1A;
const OP_LOCAL_GET: u8 = 0x20;
const OP_LOCAL_SET: u8 = 0x21;
const OP_LOCAL_TEE: u8 = 0x22;
const OP_I32_CONST: u8 = 0x41;
const OP_I64_CONST: u8 = 0x42;
const OP_I32_ADD: u8 = 0x6A;
const OP_I32_SUB: u8 = 0x6B;
const OP_I32_MUL: u8 = 0x6C;
const OP_I32_DIV_S: u8 = 0x6D;
const OP_I64_ADD: u8 = 0x7C;
const OP_I64_EXTEND_I32_S: u8 = 0xAC;

/// 把一条已解码指令映射为执行子集指令。
pub fn from_decoded_instruction(instruction: &DecodedInstruction) -> Result<Instruction, WasmError> {
    let mapped = match instruction.opcode {
        OP_RETURN => Instruction::Return,
        OP_DROP => Instruction::Drop,
        OP_I32_ADD => Instruction::I32Add,
        OP_I32_SUB => Instruction::I32Sub,
        OP_I32_MUL => Instruction::I32Mul,
        OP_I32_DIV_S => Instruction::I32DivS,
        OP_I64_ADD => Instruction::I64Add,
        OP_I64_EXTEND_I32_S => Instruction::I64ExtendI32S,
        OP_LOCAL_GET => Instruction::LocalGet(index_operand(instruction)?),
        OP_LOCAL_SET => Instruction::LocalSet(index_operand(instruction)?),
        OP_LOCAL_TEE => Instruction::LocalTee(index_operand(instruction)?),
        OP_I32_CONST => Instruction::I32Const(match instruction.operands.first() {
            Some(DecodedOperand::ValueI32(value)) => *value,
            _ => return Err(unsupported(instruction)),
        }),
        OP_I64_CONST => Instruction::I64Const(match instruction.operands.first() {
            Some(DecodedOperand::ValueI64(value)) => *value,
            _ => return Err(unsupported(instruction)),
        }),
        _ => return Err(unsupported(instruction)),
    };
    Ok(mapped)
}

/// 批量映射一段已解码指令。
pub fn from_decoded_program(program: &[DecodedInstruction]) -> Result<Vec<Instruction>, WasmError> {
    program.iter().map(from_decoded_instruction).collect()
}

fn index_operand(instruction: &DecodedInstruction) -> Result<usize, WasmError> {
    match instruction.operands.first() {
        Some(DecodedOperand::LocalIndex(index)) => Ok(*index as usize),
        _ => Err(unsupported(instruction)),
    }
}

fn unsupported(instruction: &DecodedInstruction) -> WasmError {
    WasmError::UnsupportedInstruction { instruction: format!("0x{:02X} {}", instruction.opcode, instruction.mnemonic) }
}
