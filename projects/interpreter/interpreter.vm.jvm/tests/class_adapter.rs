//! 接线门：格式层已解码指令经适配器映射后必须可直接执行，超集指令必须拒绝。

use data_binary_jvm::class::JvmInstruction;
use interpreter_vm_jvm::{Frame, Instruction, Jvm, JvmError, from_class_instruction, from_class_program};

#[test]
fn decoded_class_program_runs_end_to_end() {
    let decoded = vec![
        JvmInstruction::IConst(2),
        JvmInstruction::IConst(3),
        JvmInstruction::IMul,
        JvmInstruction::IConst(4),
        JvmInstruction::IAdd,
        JvmInstruction::IReturn,
    ];
    let program = from_class_program(&decoded).expect("subset mapping");
    assert_eq!(
        program,
        vec![
            Instruction::IConst(2),
            Instruction::IConst(3),
            Instruction::IMul,
            Instruction::IConst(4),
            Instruction::IAdd,
            Instruction::IReturn,
        ]
    );
    let mut frame = Frame::new(4, 16);
    let (value, report) = Jvm::default().execute(&program, &mut frame, 100).expect("execute");
    assert_eq!(value, 10);
    assert_eq!(report.steps, 6);
}

#[test]
fn long_constants_and_locals_map() {
    let decoded = vec![JvmInstruction::LConst1, JvmInstruction::LStore(0), JvmInstruction::LLoad(0), JvmInstruction::LReturn];
    let program = from_class_program(&decoded).expect("subset mapping");
    let mut frame = Frame::new(4, 16);
    assert!(Jvm::default().execute(&program, &mut frame, 100).is_ok());
    assert_eq!(frame.local(0), Ok(interpreter_vm_jvm::Slot::LongHigh(1)));
}

#[test]
fn instructions_outside_the_subset_are_rejected_not_dropped() {
    let error = from_class_instruction(&JvmInstruction::ALoad0).expect_err("reference load is outside the subset");
    assert!(matches!(error, JvmError::UnsupportedInstruction { .. }));

    let error = from_class_instruction(&JvmInstruction::Label("start".into())).expect_err("labels are not executable");
    assert!(matches!(error, JvmError::UnsupportedInstruction { .. }));

    let program = from_class_program(&[JvmInstruction::IConst(1), JvmInstruction::AConstNull]);
    assert!(program.is_err(), "a program containing an unsupported instruction must fail as a whole");
}
