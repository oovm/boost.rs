use hardware_frontend::FrontendError;
use hardware_input_glsl::lower_glsl;
use hardware_program::{CpuInterpreter, ScalarTy, Ty, Value};

fn fill_source() -> &'static str {
    r#"
layout(local_size_x = 4) in;
layout(std430, binding = 0) buffer Data {
    float values[];
};
void main() {
    values[gl_GlobalInvocationID.x] = 1.5;
}
"#
}

#[test]
fn lowers_compute_fill_and_runs_on_cpu() {
    let lowered = lower_glsl(fill_source()).expect("valid GLSL");
    assert_eq!(lowered.entry, "main");
    assert_eq!(lowered.resources.len(), 1);

    let binding = &lowered.resources[0];
    assert_eq!(binding.name, "values");
    assert_eq!(binding.group, 0);
    assert_eq!(binding.binding, 0);
    assert_eq!(binding.element, Ty::Scalar(ScalarTy::F32));

    let mut interpreter = CpuInterpreter::new();
    interpreter.with_buffer(0, vec![Value::F32(0.0); 4]);
    interpreter.run_parallel(&lowered.program, 4).expect("CPU execution");
    assert_eq!(interpreter.buffer(0), Some(vec![Value::F32(1.5); 4].as_slice()));
}

#[test]
fn lowers_for_loop_with_prefix_increment() {
    let source = r#"
layout(local_size_x = 2) in;
layout(std430, binding = 0) buffer Input { float values[]; };
layout(std430, binding = 1) buffer Output { float results[]; };
void main() {
    float acc = 0.0;
    for (uint k = 0u; k < 4u; ++k) {
        acc += values[k];
    }
    results[gl_GlobalInvocationID.x] = acc;
}
"#;
    let lowered = lower_glsl(source).expect("valid GLSL");
    let mut interpreter = CpuInterpreter::new();
    interpreter.with_buffer(0, vec![Value::F32(1.0), Value::F32(2.0), Value::F32(3.0), Value::F32(4.0)]);
    interpreter.with_buffer(1, vec![Value::F32(0.0); 2]);
    interpreter.run_parallel(&lowered.program, 2).expect("CPU execution");
    assert_eq!(interpreter.buffer(1), Some(vec![Value::F32(10.0); 2].as_slice()));
}

#[test]
fn lowers_uniform_block_into_slots_and_runs_on_cpu() {
    let source = r#"
layout(local_size_x = 1) in;
layout(std140, binding = 0) uniform params {
    float scale;
    float offset;
};
layout(std430, binding = 0) buffer Data { float values[]; };
void main() {
    values[gl_GlobalInvocationID.x] = params.scale * 2.0;
}
"#;
    let lowered = lower_glsl(source).expect("valid GLSL");
    assert_eq!(lowered.uniforms.len(), 1);

    let uniform = &lowered.uniforms[0];
    assert_eq!(uniform.name, "params");
    assert_eq!(uniform.group, 0);
    assert_eq!(uniform.binding, 0);
    assert_eq!(uniform.slot, 0);
    assert_eq!(uniform.fields, vec![("scale".to_owned(), Ty::Scalar(ScalarTy::F32)), ("offset".to_owned(), Ty::Scalar(ScalarTy::F32))]);

    let mut interpreter = CpuInterpreter::new();
    interpreter.with_uniform(0, Value::F32(3.0));
    interpreter.with_uniform(1, Value::F32(1.0));
    interpreter.with_buffer(0, vec![Value::F32(0.0); 1]);
    interpreter.run_parallel(&lowered.program, 1).expect("CPU execution");
    assert_eq!(interpreter.buffer(0), Some(vec![Value::F32(6.0)].as_slice()));
}

#[test]
fn rejects_unknown_uniform_field() {
    let source = r#"
layout(local_size_x = 1) in;
layout(std140, binding = 0) uniform params { float scale; };
layout(std430, binding = 0) buffer Data { float values[]; };
void main() {
    values[gl_GlobalInvocationID.x] = params.missing;
}
"#;
    let error = lower_glsl(source).expect_err("an unknown uniform field must fail");
    assert!(matches!(error, FrontendError::Unsupported(message) if message.contains("missing")));
}

#[test]
fn rejects_uniform_blocks_with_compound_members() {
    let source = r#"
layout(local_size_x = 1) in;
layout(std140, binding = 0) uniform params { vec4 tint; };
layout(std430, binding = 0) buffer Data { float values[]; };
void main() {
    values[gl_GlobalInvocationID.x] = params.tint.x;
}
"#;
    let error = lower_glsl(source).expect_err("non-scalar uniform members are not lowered yet");
    assert!(matches!(error, FrontendError::Unsupported(message) if message.contains("uniform")));
}
