use hardware_emitter::{emit_dxil, emit_program_dxbc, emit_raster_tri_dxbc, emit_vertex_raster_dxbc};
use hardware_types::ShaderProgram;

#[test]
fn emitted_compute_shaders_pass_structural_validation() {
    for program in [ShaderProgram::dispatch_fill("ucf_fill", 1.0), ShaderProgram::matmul("ucf_matmul")] {
        let bytes = emit_dxil(&program).expect("DXBC emission");
        assert_eq!(&bytes[0..4], b"DXBC");
        data_binary_dxbc::validate_dxbc_bytes(&bytes).expect("structurally valid DXBC");
    }
}

#[test]
fn emitted_generic_program_passes_structural_validation() {
    use hardware_program::{BinOp, FunctionBuilder, ProgramBuilder, Ty};

    let mut builder = FunctionBuilder::new("ucf_program_main");
    let index = builder.emit_global_id().unwrap();
    let value = builder.emit_load_buffer(0, index, Ty::f32()).unwrap();
    let scale = builder.emit_load_uniform(0, Ty::f32()).unwrap();
    let result = builder.emit_binary(BinOp::Mul, value, scale).unwrap();
    builder.return_value(result).unwrap();
    let program = ProgramBuilder::new().entry(builder).unwrap().finish().unwrap();
    let bytes = emit_program_dxbc(&program, "ucf_program_main").expect("DXBC emission");
    assert_eq!(&bytes[0..4], b"DXBC");
    data_binary_dxbc::validate_dxbc_bytes(&bytes).expect("structurally valid DXBC");
}

#[test]
fn emitted_integer_conversions_pass_structural_validation() {
    use hardware_program::{BinOp, FunctionBuilder, ProgramBuilder, Rounding, ScalarTy, Ty, UnOp};

    let u32_ty = Ty::Scalar(ScalarTy::U32);
    let mut builder = FunctionBuilder::new("ucf_integer_main");
    let gid = builder.emit_global_id().unwrap();
    let word = builder.emit_load_buffer(0, gid, u32_ty.clone()).unwrap();
    let uniform_word = builder.emit_load_uniform(0, u32_ty.clone()).unwrap();
    let as_float = builder.emit_unary(UnOp::U32ToF32, word, Ty::f32()).unwrap();
    let truncated = builder.emit_unary(UnOp::F32ToU32(Rounding::Trunc), as_float, u32_ty.clone()).unwrap();
    let rounded = builder.emit_unary(UnOp::F32ToU32(Rounding::Round), as_float, u32_ty.clone()).unwrap();
    let combined = builder.emit_binary(BinOp::Add, truncated, rounded).unwrap();
    let mask = builder.emit_const_u32(0xff).unwrap();
    let masked = builder.emit_binary(BinOp::And, combined, mask).unwrap();
    let ored = builder.emit_binary(BinOp::Or, masked, uniform_word).unwrap();
    let three = builder.emit_const_u32(3).unwrap();
    let shifted = builder.emit_binary(BinOp::Shl, ored, three).unwrap();
    let back = builder.emit_binary(BinOp::Shr, shifted, three).unwrap();
    let bits = builder.emit_unary(UnOp::Bitcast, back, Ty::f32()).unwrap();
    builder.return_value(bits).unwrap();
    let program = ProgramBuilder::new().entry(builder).unwrap().finish().unwrap();
    let bytes = emit_program_dxbc(&program, "ucf_integer_main").expect("DXBC emission");
    assert_eq!(&bytes[0..4], b"DXBC");
    data_binary_dxbc::validate_dxbc_bytes(&bytes).expect("structurally valid DXBC");
}

#[test]
fn emitted_raster_triangle_passes_structural_validation() {
    let triangle = emit_raster_tri_dxbc().expect("DXBC emission");
    for bytes in [&triangle.vs, &triangle.ps] {
        assert_eq!(&bytes[0..4], b"DXBC");
        data_binary_dxbc::validate_dxbc_bytes(bytes).expect("structurally valid DXBC");
    }
}

#[test]
fn emitted_vertex_raster_passes_structural_validation() {
    let vertex = emit_vertex_raster_dxbc().expect("DXBC emission");
    for bytes in [&vertex.vs, &vertex.ps] {
        assert_eq!(&bytes[0..4], b"DXBC");
        data_binary_dxbc::validate_dxbc_bytes(bytes).expect("structurally valid DXBC");
    }
}
