//! 执行门：数值栈、局部变量、预算与 WASI/component-model 边界必须按合同工作。

use interpreter_vm_wasm::{
    ComponentDirection, HostBoundary, ImportKind, Instruction, WasmError, WasmValue, WasmVm, classify, is_wasi_preview1_import,
    wasi_preview1_import,
};

fn new_vm() -> WasmVm {
    WasmVm::new(vec![WasmValue::I32(0), WasmValue::I32(0)], 8)
}

#[test]
fn i32_arithmetic_returns_value() {
    let mut vm = new_vm();
    let code = [Instruction::I32Const(2), Instruction::I32Const(3), Instruction::I32Mul, Instruction::I32Const(4), Instruction::I32Add, Instruction::Return];
    assert_eq!(vm.execute(&code, 100).map(|(value, _)| value), Ok(WasmValue::I32(10)));
}

#[test]
fn local_tee_writes_and_keeps_value() {
    let mut vm = new_vm();
    let code = [Instruction::I32Const(7), Instruction::LocalTee(0), Instruction::LocalGet(0), Instruction::I32Add, Instruction::Return];
    assert_eq!(vm.execute(&code, 100).map(|(value, _)| value), Ok(WasmValue::I32(14)));
}

#[test]
fn i64_extension_is_sign_preserving() {
    let mut vm = new_vm();
    let code = [Instruction::I32Const(-2), Instruction::I64ExtendI32S, Instruction::I64Const(2), Instruction::I64Add, Instruction::Return];
    assert_eq!(vm.execute(&code, 100).map(|(value, _)| value), Ok(WasmValue::I64(0)));
}

#[test]
fn divide_by_zero_and_budget_are_reported() {
    let mut vm = new_vm();
    let code = [Instruction::I32Const(1), Instruction::I32Const(0), Instruction::I32DivS, Instruction::Return];
    assert_eq!(vm.execute(&code, 100), Err(WasmError::DivideByZero));

    let mut vm = new_vm();
    assert_eq!(vm.execute(&[Instruction::Nop, Instruction::Nop], 1), Err(WasmError::StepBudgetExceeded { budget: 1 }));
}

#[test]
fn stack_limit_is_enforced() {
    let mut vm = WasmVm::new(Vec::new(), 2);
    let code = [Instruction::I32Const(1), Instruction::I32Const(2), Instruction::I32Const(3)];
    assert_eq!(vm.execute(&code, 100), Err(WasmError::StackOverflow { limit: 2 }));
}

#[test]
fn wasi_preview1_names_resolve_and_unknown_field_is_rejected() {
    assert!(is_wasi_preview1_import("wasi_snapshot_preview1.fd_write"));
    assert_eq!(wasi_preview1_import("wasi_snapshot_preview1.proc_exit"), Ok(Some(interpreter_vm_wasm::WasiPreview1Import::ProcExit)));
    assert_eq!(wasi_preview1_import("env.memory"), Ok(None));
    assert_eq!(
        wasi_preview1_import("wasi_snapshot_preview1.not_a_function"),
        Err(WasmError::UnsupportedImport { name: "wasi_snapshot_preview1.not_a_function".into() })
    );
}

#[test]
fn filesystem_capability_is_classified() {
    let mut boundary = HostBoundary::new();
    boundary.declare("wasi_snapshot_preview1.fd_write").expect("known");
    boundary.declare("wasi_snapshot_preview1.random_get").expect("known");
    assert_eq!(boundary.len(), 2);
    assert_eq!(boundary.filesystem_imports(), vec!["wasi_snapshot_preview1.fd_write"]);
}

#[test]
fn component_model_direction_and_package_are_parsed() {
    assert_eq!(
        classify("wasi:cli/run@0.2.0#import"),
        Ok(ImportKind::ComponentModel { direction: ComponentDirection::Import, package: "wasi:cli".into() })
    );
    assert_eq!(
        classify("wasi:http/types@0.2.0#export"),
        Ok(ImportKind::ComponentModel { direction: ComponentDirection::Export, package: "wasi:http".into() })
    );
    assert_eq!(classify("env.memory"), Ok(ImportKind::Host { module: "env".into(), field: "memory".into() }));
    assert_eq!(classify("no_separator"), Err(WasmError::UnsupportedImport { name: "no_separator".into() }));
}
