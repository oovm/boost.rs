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
fn rejects_uniform_blocks() {
    let source = r#"
layout(local_size_x = 1) in;
layout(std140) uniform Params { float scale; };
layout(std430, binding = 0) buffer Data { float values[]; };
void main() {
    values[gl_GlobalInvocationID.x] = scale;
}
"#;
    let error = lower_glsl(source).expect_err("uniform blocks are not lowered yet");
    assert!(error.to_string().contains("uniform"));
}
