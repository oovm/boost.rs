use hardware_emitter::{CodegenError, CodegenOptions, TargetIsa, encode_program, encode_shader};
use hardware_program::{Expr, Function, Program, Stmt, Ty};
use hardware_types::ShaderProgram;

#[test]
fn encodes_fixed_operations_through_the_unified_entry() {
    let fill = encode_shader(&ShaderProgram::dispatch_fill("ucf_fill", 1.0), TargetIsa::Ptx).expect("PTX fill");
    assert_eq!(fill.isa, TargetIsa::Ptx);
    assert!(!fill.bytes.is_empty());

    let matmul = encode_shader(&ShaderProgram::matmul("ucf_matmul"), TargetIsa::Ptx).expect("PTX matmul");
    assert_eq!(matmul.isa, TargetIsa::Ptx);
}

#[test]
fn encodes_generic_programs_through_the_unified_entry() {
    let program = Program {
        version: 1,
        functions: vec![Function {
            name: "fill".into(),
            params: Vec::new(),
            locals: Vec::new(),
            body: vec![Stmt::Return(Expr::ConstF32(1.0))],
            ret: Ty::f32(),
        }],
        entry: 0,
    };
    let module = encode_program(&program, "fill", TargetIsa::Spirv, CodegenOptions::default()).expect("SPIR-V");
    assert_eq!(module.isa, TargetIsa::Spirv);
    assert_eq!(&module.bytes[0..4], &0x0723_0203u32.to_le_bytes());
}

#[test]
fn reports_missing_program_encoders_explicitly() {
    let program = Program {
        version: 1,
        functions: vec![Function {
            name: "fill".into(),
            params: Vec::new(),
            locals: Vec::new(),
            body: vec![Stmt::Return(Expr::ConstF32(1.0))],
            ret: Ty::f32(),
        }],
        entry: 0,
    };
    let error = encode_program(&program, "fill", TargetIsa::Dxbc, CodegenOptions::default()).expect_err("no DXBC program encoder");
    assert!(matches!(error, CodegenError::Unsupported { isa: TargetIsa::Dxbc, .. }));
}
