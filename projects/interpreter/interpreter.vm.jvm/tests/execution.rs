//! 执行门：槽宽、算术、局部变量、步进预算与 native 边界必须按 JVM 语义工作。

use interpreter_vm_jvm::{Frame, Instruction, Jvm, JvmError, NativeRegistry};

fn run(code: &[Instruction]) -> Result<i32, JvmError> {
    let mut frame = Frame::new(8, 16);
    Jvm::default().execute(code, &mut frame, 1_000).map(|(value, _)| value)
}

#[test]
fn integer_arithmetic_returns_top_of_stack() {
    let code =
        [Instruction::IConst(2), Instruction::IConst(3), Instruction::IMul, Instruction::IConst(4), Instruction::IAdd, Instruction::IReturn];
    assert_eq!(run(&code), Ok(10));
}

#[test]
fn long_occupies_two_slots_and_round_trips_through_locals() {
    let code = [
        Instruction::LConst(7),
        Instruction::LStore(0),
        Instruction::LLoad(0),
        Instruction::IConst(1),
        Instruction::I2L,
        Instruction::LAdd,
        Instruction::LReturn,
    ];
    let mut frame = Frame::new(8, 16);
    assert_eq!(Jvm::default().execute(&code, &mut frame, 100), Ok((0, interpreter_vm_jvm::vm::StepReport { steps: 7, returned: true })));
    assert_eq!(frame.local(0), Ok(interpreter_vm_jvm::Slot::LongHigh(7)));
    assert_eq!(frame.local(1), Ok(interpreter_vm_jvm::Slot::LongLow));
}

#[test]
fn divide_by_zero_is_reported() {
    let code = [Instruction::IConst(1), Instruction::IConst(0), Instruction::IDiv, Instruction::IReturn];
    assert_eq!(run(&code), Err(JvmError::DivideByZero));
}

#[test]
fn step_budget_stops_runaway_program() {
    let code = [Instruction::Nop, Instruction::Nop, Instruction::Nop];
    let mut frame = Frame::new(1, 4);
    assert_eq!(Jvm::default().execute(&code, &mut frame, 2), Err(JvmError::StepBudgetExceeded { budget: 2 }));
}

#[test]
fn stack_limit_rejects_overflow() {
    let code = [Instruction::IConst(1), Instruction::IConst(2), Instruction::IConst(3)];
    let mut frame = Frame::new(1, 2);
    assert_eq!(Jvm::default().execute(&code, &mut frame, 100), Err(JvmError::StackOverflow { limit: 2 }));
}

#[test]
fn registered_native_is_callable_and_unbound_is_reported() {
    fn double(args: &[i32]) -> Option<i32> {
        args.first().map(|value| value * 2)
    }
    let mut natives = NativeRegistry::new();
    natives.register("host.double", double);
    let code = [Instruction::IConst(21), Instruction::InvokeStaticNative { name: "host.double".into() }, Instruction::IReturn];
    let mut frame = Frame::new(1, 4);
    assert_eq!(Jvm::new(natives).execute(&code, &mut frame, 100).map(|(value, _)| value), Ok(42));

    let missing = [Instruction::IConst(1), Instruction::InvokeStaticNative { name: "host.missing".into() }];
    let mut frame = Frame::new(1, 4);
    assert_eq!(Jvm::default().execute(&missing, &mut frame, 100), Err(JvmError::UnboundNative { name: "host.missing".into() }));
}

#[test]
fn local_slot_out_of_range_is_rejected() {
    let code = [Instruction::ILoad(4)];
    let mut frame = Frame::new(2, 4);
    assert_eq!(Jvm::default().execute(&code, &mut frame, 100), Err(JvmError::LocalOutOfRange { index: 4, slots: 2 }));
}
