//! UCF 新增整数/转换构造的构造、校验与 CPU 解释器语义测试。
//!
//! 语义对齐基准：media 侧 CPU 参考的 Rust `as` 转换——
//! - `f32 as u32`：向零截断并饱和（NaN 与负数得 0，超界得 `u32::MAX`）；
//! - `u32 as f32`：IEEE 就近取偶舍入；
//! - `.round()`：就近舍入、平局远离零（Rust `f32::round`），因此 `F32ToU32(Round)`
//!   选择平局远离零而非平局取偶，保证 media 现有 `.round() as u8/u32` 写法逐位一致。

use hardware_program::{
    BinOp, BuildError, CfgBlock, CfgInstruction, CfgTerminator, CpuInterpreter, Function, FunctionBuilder, Program, ProgramBuilder, Rounding,
    ScalarTy, Stmt, Ty, UnOp, Value, validate_program,
};

fn u32_ty() -> Ty {
    Ty::Scalar(ScalarTy::U32)
}

fn run(program: &Program, count: u32) -> Vec<Value> {
    let mut interpreter = CpuInterpreter::new();
    interpreter
        .run_parallel(program, count)
        .expect("execute")
        .iter()
        .map(|result| result.value.clone())
        .collect()
}

fn run_bound(program: &Program, buffers: Vec<(u32, Vec<Value>)>, count: u32) -> Vec<Value> {
    let mut interpreter = CpuInterpreter::new();
    for (resource, values) in buffers {
        interpreter.with_buffer(resource, values);
    }
    interpreter
        .run_parallel(program, count)
        .expect("execute")
        .iter()
        .map(|result| result.value.clone())
        .collect()
}

#[test]
fn bitcast_reinterprets_bits_without_changing_them() {
    let mut builder = FunctionBuilder::new("bitcast_roundtrip");
    let value = builder.emit_const_f32(-0.0).expect("const");
    let bits = builder.emit_unary(UnOp::Bitcast, value, u32_ty()).expect("to bits");
    let back = builder.emit_unary(UnOp::Bitcast, bits, Ty::f32()).expect("from bits");
    let bits_back = builder.emit_unary(UnOp::Bitcast, back, u32_ty()).expect("bits again");
    builder.return_value(bits_back).expect("return");
    let program = ProgramBuilder::new().entry(builder).expect("entry").finish().expect("program");
    validate_program(&program).expect("valid");
    // -0.0 的位模式 0x8000_0000 必须逐位往返（== 比较无法区分 ±0，故断言位值）。
    assert_eq!(run(&program, 1), vec![Value::U32(0x8000_0000)]);

    // 带载荷的 NaN 也按位往返，不被规范化。
    let mut builder = FunctionBuilder::new("bitcast_nan");
    let value = builder.emit_const_f32(f32::from_bits(0x7fc0_0001)).expect("const");
    let bits = builder.emit_unary(UnOp::Bitcast, value, u32_ty()).expect("to bits");
    let expected_bits = builder.emit_const_u32(0x7fc0_0001).expect("const");
    let check = builder.emit_binary(BinOp::Eq, bits, expected_bits).expect("eq");
    let back = builder.emit_unary(UnOp::Bitcast, bits, Ty::f32()).expect("from bits");
    let bits_back = builder.emit_unary(UnOp::Bitcast, back, u32_ty()).expect("to bits again");
    let same = builder.emit_binary(BinOp::Eq, bits, bits_back).expect("same");
    let both = builder.emit_binary(BinOp::And, check, same);
    assert!(matches!(both, Err(BuildError::TypeMismatch { .. })), "Bool 不再允许进入整数 And");
    builder.return_value(same).expect("return");
    let program = ProgramBuilder::new().entry(builder).expect("entry").finish().expect("program");
    validate_program(&program).expect("valid");
    // Bool 结果在 CPU 侧是 Value::Bool(true)。
    assert_eq!(run(&program, 1), vec![Value::Bool(true)]);
}

#[test]
fn u32_to_f32_matches_rust_as_conversion() {
    let cases: [u32; 6] = [0, 1, 1 << 24, (1 << 24) + 1, (1 << 24) + 2, u32::MAX];
    let mut builder = FunctionBuilder::new("u32_to_f32");
    let mut outs = Vec::new();
    for value in cases {
        let input = builder.emit_const_u32(value).expect("const");
        let converted = builder.emit_unary(UnOp::U32ToF32, input, Ty::f32()).expect("convert");
        let bits = builder.emit_unary(UnOp::Bitcast, converted, u32_ty()).expect("bits");
        outs.push((value, bits));
    }
    // 用位模式做精确断言：最后一项的位结果直接返回，其余在下方与 Rust 对照。
    let last = outs.last().expect("case").1;
    builder.return_value(last).expect("return");
    let program = ProgramBuilder::new().entry(builder).expect("entry").finish().expect("program");
    validate_program(&program).expect("valid");
    let got = run(&program, 1);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0], Value::U32((cases[cases.len() - 1] as f32).to_bits()), "u32::MAX -> 4294967296.0");

    // 逐例对照 Rust `as`（IEEE 就近取偶）。
    for (value, bits) in outs {
        let mut builder = FunctionBuilder::new("u32_to_f32_case");
        let input = builder.emit_const_u32(value).expect("const");
        let converted = builder.emit_unary(UnOp::U32ToF32, input, Ty::f32()).expect("convert");
        let out = builder.emit_unary(UnOp::Bitcast, converted, u32_ty()).expect("bits");
        builder.return_value(out).expect("return");
        let program = ProgramBuilder::new().entry(builder).expect("entry").finish().expect("program");
        validate_program(&program).expect("valid");
        assert_eq!(run(&program, 1), vec![Value::U32((value as f32).to_bits())], "case {value}");
        let _ = bits;
    }
}

#[test]
fn f32_to_u32_trunc_saturates_like_rust_as() {
    // 与 Rust `f32 as u32`（饱和、向零）逐位对照。
    let cases: [f32; 9] = [
        0.0,
        -0.0,
        2.7,
        -2.7,
        -1.0,
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        4294967040.0,
    ];
    for value in cases {
        let mut builder = FunctionBuilder::new("trunc_case");
        let input = builder.emit_const_f32(value).expect("const");
        let out = builder.emit_unary(UnOp::F32ToU32(Rounding::Trunc), input, u32_ty()).expect("convert");
        builder.return_value(out).expect("return");
        let program = ProgramBuilder::new().entry(builder).expect("entry").finish().expect("program");
        validate_program(&program).expect("valid");
        assert_eq!(run(&program, 1), vec![Value::U32(value as u32)], "case {value}");
    }
}

#[test]
fn f32_to_u32_round_is_ties_away_to_match_media_cpu_reference() {
    // media 侧 CPU 参考写的是 `(v).round() as u8/u32`，Rust `f32::round` 平局远离零。
    // 若选平局取偶，126.5 会得 126 而 CPU 得 127，破坏逐字节 parity；故 Round = 远离零。
    let cases: [f32; 8] = [0.5, 1.5, 2.5, 3.5, 126.5, 127.5, -2.5, f32::NAN];
    for value in cases {
        let mut builder = FunctionBuilder::new("round_case");
        let input = builder.emit_const_f32(value).expect("const");
        let out = builder.emit_unary(UnOp::F32ToU32(Rounding::Round), input, u32_ty()).expect("convert");
        builder.return_value(out).expect("return");
        let program = ProgramBuilder::new().entry(builder).expect("entry").finish().expect("program");
        validate_program(&program).expect("valid");
        assert_eq!(run(&program, 1), vec![Value::U32(value.round() as u32)], "case {value}");
    }
    // 显式钉死平局差异点：126.5 必须得 127（平局取偶会得 126）。
    let mut builder = FunctionBuilder::new("tie_126_5");
    let input = builder.emit_const_f32(126.5).expect("const");
    let out = builder.emit_unary(UnOp::F32ToU32(Rounding::Round), input, u32_ty()).expect("convert");
    builder.return_value(out).expect("return");
    let program = ProgramBuilder::new().entry(builder).expect("entry").finish().expect("program");
    assert_eq!(run(&program, 1), vec![Value::U32(127)]);
}

#[test]
fn integer_bitwise_and_shift_ops_wrap_and_saturate_shifts() {
    let word: u32 = 0xdead_beef;
    let cases: [(BinOp, u32, u32); 8] = [
        (BinOp::And, word, 0x0000_ffff),
        (BinOp::Or, 0xf0f0_0f0f, 0x0000_ffff),
        (BinOp::Shl, 0x0000_0001, 4),
        (BinOp::Shl, 0xffff_ffff, 31),
        // 移位量 >= 32 在 DXBC 未定义；IR 层约定为 0，并在真机测试验证。
        (BinOp::Shl, 0xffff_ffff, 32),
        (BinOp::Shl, 0xffff_ffff, 64),
        (BinOp::Shr, word, 8),
        (BinOp::Shr, word, 33),
    ];
    for (op, lhs, rhs) in cases {
        let mut builder = FunctionBuilder::new("bit_case");
        let left = builder.emit_const_u32(lhs).expect("lhs");
        let right = builder.emit_const_u32(rhs).expect("rhs");
        let out = builder.emit_binary(op, left, right).expect("binary");
        builder.return_value(out).expect("return");
        let program = ProgramBuilder::new().entry(builder).expect("entry").finish().expect("program");
        validate_program(&program).expect("valid");
        let expected = match op {
            BinOp::And => lhs & rhs,
            BinOp::Or => lhs | rhs,
            BinOp::Shl => if rhs >= 32 { 0 } else { lhs << rhs },
            BinOp::Shr => if rhs >= 32 { 0 } else { lhs >> rhs },
            _ => unreachable!(),
        };
        assert_eq!(run(&program, 1), vec![Value::U32(expected)], "case {op:?} {lhs} {rhs}");
    }
}

#[test]
fn cfg_integer_load_unpacks_rgba8_bytes_like_media_cpu() {
    // 模拟 media 的 RGBA8 解包：word = 0xAABBCCDD，线程 i 取第 i 字节。
    let word: u32 = 0xdd_cc_bb_aa;
    let mut builder = FunctionBuilder::new("unpack");
    let gid = builder.emit_global_id().expect("gid");
    let input = builder.emit_load_buffer(0, gid, u32_ty()).expect("load");
    let eight = builder.emit_const_u32(8).expect("eight");
    let shift = builder.emit_binary(BinOp::Mul, gid, eight).expect("shift");
    let byte = builder.emit_binary(BinOp::Shr, input, shift).expect("shr");
    let mask = builder.emit_const_u32(0xff).expect("mask");
    let out = builder.emit_binary(BinOp::And, byte, mask).expect("and");
    builder.return_value(out).expect("return");
    let program = ProgramBuilder::new().entry(builder).expect("entry").finish().expect("program");
    validate_program(&program).expect("valid");

    let results = run_bound(&program, vec![(0, vec![Value::U32(word); 4])], 4);
    let expected: Vec<Value> = (0..4u32).map(|i| Value::U32((word >> (8 * i)) & 0xff)).collect();
    assert_eq!(results, expected);
}

#[test]
fn integer_operations_reject_float_operands_at_build_and_validate() {
    // 构建层：emit_binary 直接拒绝 Float×Float 的 And。
    let mut builder = FunctionBuilder::new("bad_and");
    let lhs = builder.emit_const_f32(1.0).expect("const");
    let rhs = builder.emit_const_f32(2.0).expect("const");
    assert!(matches!(builder.emit_binary(BinOp::And, lhs, rhs), Err(BuildError::TypeMismatch { .. })));

    // 校验层：手工构造 `And(f32, u32)` 必须被 validate_program 拒绝。
    let program = Program {
        version: 1,
        entry: 0,
        functions: vec![Function {
            name: "manual_and".into(),
            params: vec![],
            locals: vec![],
            ret: u32_ty(),
            body: vec![Stmt::Cfg {
                entry: 0,
                blocks: vec![CfgBlock {
                    id: 0,
                    parameters: vec![],
                    instructions: vec![
                        CfgInstruction::ConstF32 { result: 0, value: 1.0 },
                        CfgInstruction::ConstU32 { result: 1, value: 2 },
                        CfgInstruction::Binary { result: 2, op: BinOp::And, lhs: 0, rhs: 1 },
                    ],
                    terminator: CfgTerminator::Return(2),
                }],
            }],
        }],
    };
    assert!(validate_program(&program).is_err());
}

#[test]
fn conversion_instructions_reject_mistyped_operands() {
    // Bitcast 结果类型必须与源类型互补。
    let mut builder = FunctionBuilder::new("bad_bitcast");
    let value = builder.emit_const_u32(7).expect("const");
    assert!(matches!(builder.emit_unary(UnOp::Bitcast, value, u32_ty()), Err(BuildError::TypeMismatch { .. })));

    // U32ToF32 不接受 f32 源。
    let mut builder = FunctionBuilder::new("bad_i2f");
    let value = builder.emit_const_f32(7.0).expect("const");
    assert!(matches!(builder.emit_unary(UnOp::U32ToF32, value, Ty::f32()), Err(BuildError::TypeMismatch { .. })));

    // F32ToU32 不接受 u32 源。
    let mut builder = FunctionBuilder::new("bad_f2i");
    let value = builder.emit_const_u32(7).expect("const");
    assert!(matches!(builder.emit_unary(UnOp::F32ToU32(Rounding::Trunc), value, u32_ty()), Err(BuildError::TypeMismatch { .. })));

    // 手工构造 Bitcast(Bool) 被 validate 拒绝。
    let program = Program {
        version: 1,
        entry: 0,
        functions: vec![Function {
            name: "manual_bitcast".into(),
            params: vec![],
            locals: vec![],
            ret: u32_ty(),
            body: vec![Stmt::Cfg {
                entry: 0,
                blocks: vec![CfgBlock {
                    id: 0,
                    parameters: vec![],
                    instructions: vec![
                        CfgInstruction::ConstF32 { result: 0, value: 1.0 },
                        CfgInstruction::Unary { result: 1, op: UnOp::IsFinite, arg: 0 },
                        CfgInstruction::Unary { result: 2, op: UnOp::Bitcast, arg: 1 },
                    ],
                    terminator: CfgTerminator::Return(2),
                }],
            }],
        }],
    };
    assert!(validate_program(&program).is_err());
}

#[test]
fn interpreter_rejects_mistyped_runtime_values() {
    // 程序声明 u32 uniform，但运行时绑定非整数浮点值：整数域加载必须报错，
    // 而不是静默换标签（这正是旧 f32()/u32() 换标签路径掩盖的问题）。
    let mut builder = FunctionBuilder::new("runtime_uniform");
    let slot = builder.emit_load_uniform(0, u32_ty()).expect("load");
    let one = builder.emit_const_u32(1).expect("const");
    let out = builder.emit_binary(BinOp::Shl, slot, one).expect("shl");
    builder.return_value(out).expect("return");
    let program = ProgramBuilder::new().entry(builder).expect("entry").finish().expect("program");
    validate_program(&program).expect("valid");
    let mut interpreter = CpuInterpreter::new();
    interpreter.with_uniform(0, Value::F32(1.5));
    assert!(interpreter.run(&program).is_err());
    // 整数值的浮点绑定则可收敛为 u32。
    let mut interpreter = CpuInterpreter::new();
    interpreter.with_uniform(0, Value::F32(3.0));
    let result = interpreter.run(&program).expect("execute");
    assert_eq!(result.value, Value::U32(6));
}

#[test]
fn typed_uniform_load_flows_through_interpreter() {
    let mut builder = FunctionBuilder::new("uniform_u32");
    let slot = builder.emit_load_uniform(0, u32_ty()).expect("load");
    let mask = builder.emit_const_u32(0x00ff_ffff).expect("mask");
    let out = builder.emit_binary(BinOp::And, slot, mask).expect("and");
    builder.return_value(out).expect("return");
    let program = ProgramBuilder::new().entry(builder).expect("entry").finish().expect("program");
    validate_program(&program).expect("valid");
    let mut interpreter = CpuInterpreter::new();
    interpreter.with_uniform(0, Value::U32(0xdead_beef));
    let result = interpreter.run(&program).expect("execute");
    assert_eq!(result.value, Value::U32(0x00ad_beef));
}
