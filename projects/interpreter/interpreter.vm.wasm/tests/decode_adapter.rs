//! 接线门：格式层解码指令经适配器映射后必须可直接执行，超集指令必须拒绝。

use data_binary_wasm::instruction::{DecodedInstruction, DecodedOperand};
use interpreter_vm_wasm::{Instruction, WasmError, WasmValue, WasmVm, from_decoded_instruction, from_decoded_program};

fn ins(opcode: u8, mnemonic: &'static str, operands: Vec<DecodedOperand>) -> DecodedInstruction {
    DecodedInstruction { offset: 0, opcode, mnemonic: mnemonic.into(), operands, raw_size: 1 }
}

#[test]
fn decoded_program_runs_end_to_end() {
    let decoded = vec![
        ins(0x41, "i32.const", vec![DecodedOperand::ValueI32(2)]),
        ins(0x41, "i32.const", vec![DecodedOperand::ValueI32(3)]),
        ins(0x6C, "i32.mul", vec![]),
        ins(0x41, "i32.const", vec![DecodedOperand::ValueI32(4)]),
        ins(0x6A, "i32.add", vec![]),
        ins(0x0F, "return", vec![]),
    ];
    let program = from_decoded_program(&decoded).expect("subset mapping");
    assert_eq!(
        program,
        vec![
            Instruction::I32Const(2),
            Instruction::I32Const(3),
            Instruction::I32Mul,
            Instruction::I32Const(4),
            Instruction::I32Add,
            Instruction::Return,
        ]
    );
    let mut vm = WasmVm::new(Vec::new(), 8);
    assert_eq!(vm.execute(&program, 100).map(|(value, _)| value), Ok(WasmValue::I32(10)));
}

#[test]
fn local_access_and_i64_extension_map() {
    let decoded = vec![
        ins(0x41, "i32.const", vec![DecodedOperand::ValueI32(-2)]),
        ins(0x21, "local.set", vec![DecodedOperand::LocalIndex(0)]),
        ins(0x20, "local.get", vec![DecodedOperand::LocalIndex(0)]),
        ins(0xAC, "i64.extend_i32_s", vec![]),
        ins(0x42, "i64.const", vec![DecodedOperand::ValueI64(2)]),
        ins(0x7C, "i64.add", vec![]),
        ins(0x0F, "return", vec![]),
    ];
    let program = from_decoded_program(&decoded).expect("subset mapping");
    let mut vm = WasmVm::new(vec![WasmValue::I32(0)], 8);
    assert_eq!(vm.execute(&program, 100).map(|(value, _)| value), Ok(WasmValue::I64(0)));
}

#[test]
fn instructions_outside_the_subset_are_rejected_not_dropped() {
    let memory_load = ins(0x28, "i32.load", vec![DecodedOperand::MemArg { align: 2, offset: 0 }]);
    assert!(matches!(from_decoded_instruction(&memory_load), Err(WasmError::UnsupportedInstruction { .. })));

    let missing_immediate = ins(0x41, "i32.const", vec![]);
    assert!(matches!(from_decoded_instruction(&missing_immediate), Err(WasmError::UnsupportedInstruction { .. })));

    let program = from_decoded_program(&[ins(0x41, "i32.const", vec![DecodedOperand::ValueI32(1)]), memory_load]);
    assert!(program.is_err(), "a program containing an unsupported instruction must fail as a whole");
}
