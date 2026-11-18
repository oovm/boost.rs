//! 基线标量驱动的合同测试：以 mock 格式适配器验证通用调度，不绑定任何字节码格式。

use interpreter_jit::{
    BaselineScalarJit, I32Binop, JitAssumption, JitCompileRequest, JitCompiler, JitError, JitFunctionSpec, ScalarProgram, ScalarProgramMatcher,
    decode_scalar_program, encode_ret_const_i32,
};

struct ConstMatcher(i32);

impl ScalarProgramMatcher for ConstMatcher {
    fn match_scalar_program(&self, request: &JitCompileRequest) -> Option<Vec<u8>> {
        let _ = request.function_code().ok()?;
        Some(encode_ret_const_i32(self.0))
    }
}

struct RejectMatcher;

impl ScalarProgramMatcher for RejectMatcher {
    fn match_scalar_program(&self, _request: &JitCompileRequest) -> Option<Vec<u8>> {
        None
    }
}

fn request() -> JitCompileRequest {
    JitCompileRequest {
        module_version: 7,
        module_name: "mock-module".to_string(),
        code_bytes: vec![0u8; 8],
        constant_i32: vec![Some(1)],
        function_index: 2,
        function: JitFunctionSpec { code_offset: 0, code_length: 8, local_count: 3, arity: 0, safepoint_indices: vec![1, 1, 4] },
    }
}

#[test]
fn matched_program_gets_blob_stack_maps_deopt_and_assumptions() {
    let mut jit = BaselineScalarJit::new(ConstMatcher(42));
    let artifact = jit.compile_function(&request()).expect("mock matcher compiles");
    assert_eq!(artifact.function_index, 2);
    let program = decode_scalar_program(artifact.machine_code.as_ref().expect("blob attached")).expect("blob decodes");
    assert_eq!(program, ScalarProgram::RetConstI32 { value: 42 });
    assert_eq!(artifact.stack_maps.entries.len(), 2, "safepoint 下标去重后两条目");
    assert_eq!(artifact.deopt_map.entries.len(), 2);
    assert!(artifact.assumptions.contains(&JitAssumption::ScalarLeafShape));
    assert!(artifact.assumptions.contains(&JitAssumption::ModuleVersion(7)));
}

#[test]
fn unmatched_program_is_unsupported() {
    let mut jit = BaselineScalarJit::new(RejectMatcher);
    assert_eq!(jit.compile_function(&request()), Err(JitError::Unsupported));
}

#[test]
fn invalid_function_range_is_rejected_before_matching() {
    let mut bad = request();
    bad.function.code_length = 64;
    let mut jit = BaselineScalarJit::new(ConstMatcher(1));
    assert!(matches!(jit.compile_function(&bad), Err(JitError::InvalidBytecode(_))));
}

#[test]
fn default_requires_matcher_default_and_binop_contract_is_stable() {
    #[derive(Default)]
    struct DefaultMatcher;
    impl ScalarProgramMatcher for DefaultMatcher {
        fn match_scalar_program(&self, _request: &JitCompileRequest) -> Option<Vec<u8>> {
            Some(interpreter_jit::encode_ret_i32_binop_locals(I32Binop::Add, 0, 1))
        }
    }
    let mut jit = BaselineScalarJit::<DefaultMatcher>::default();
    let artifact = jit.compile_function(&request()).expect("default-constructed backend compiles");
    match decode_scalar_program(artifact.machine_code.as_ref().unwrap()).unwrap() {
        ScalarProgram::RetI32BinopLocals { binop, a, b } => assert_eq!((binop, a, b), (I32Binop::Add, 0, 1)),
        other => panic!("unexpected program: {other:?}"),
    }
}
