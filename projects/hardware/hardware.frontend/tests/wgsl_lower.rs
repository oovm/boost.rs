use hardware_frontend::{FrontendError, lower_wgsl};
use hardware_program::{CpuInterpreter, ScalarTy, Ty, Value};

fn fill_source() -> &'static str {
    r#"
@group(0) @binding(0) var<storage, read_write> out: array<f32>;

@compute @workgroup_size(4, 1, 1)
fn fill(@builtin(global_invocation_id) gid: vec3<u32>) {
    let index = gid.x;
    out[index] = 1.5;
}
"#
}

#[test]
fn lowers_compute_fill_and_runs_on_cpu() {
    let lowered = lower_wgsl(fill_source()).expect("valid WGSL");
    assert_eq!(lowered.entry, "fill");
    assert_eq!(lowered.resources.len(), 1);

    let binding = &lowered.resources[0];
    assert_eq!(binding.name, "out");
    assert_eq!(binding.group, 0);
    assert_eq!(binding.binding, 0);
    assert_eq!(binding.resource, 0);
    assert_eq!(binding.element, Ty::Scalar(ScalarTy::F32));

    let mut interpreter = CpuInterpreter::new();
    interpreter.with_buffer(0, vec![Value::F32(0.0); 4]);
    let results = interpreter.run_parallel(&lowered.program, 4).expect("CPU execution");
    assert_eq!(results.len(), 4);
    assert_eq!(interpreter.buffer(0), Some(vec![Value::F32(1.5); 4].as_slice()));
}

#[test]
fn lowers_bounded_loop_and_buffer_reads() {
    let source = r#"
@group(0) @binding(0) var<storage, read> a: array<f32>;
@group(0) @binding(1) var<storage, read_write> out: array<f32>;

@compute @workgroup_size(4, 1, 1)
fn sum(@builtin(global_invocation_id) gid: vec3<u32>) {
    let index = gid.x;
    var acc = 0.0;
    for (var k = 0u; k < 4u; k = k + 1u) {
        acc = acc + a[k];
    }
    out[index] = acc;
}
"#;
    let lowered = lower_wgsl(source).expect("valid WGSL");
    assert_eq!(lowered.resources.len(), 2);
    assert_eq!(lowered.resources[1].binding, 1);

    let mut interpreter = CpuInterpreter::new();
    interpreter.with_buffer(0, vec![Value::F32(1.0), Value::F32(2.0), Value::F32(3.0), Value::F32(4.0)]);
    interpreter.with_buffer(1, vec![Value::F32(0.0); 2]);
    interpreter.run_parallel(&lowered.program, 2).expect("CPU execution");
    assert_eq!(interpreter.buffer(1), Some(vec![Value::F32(10.0); 2].as_slice()));
}

#[test]
fn lowers_conditionals_and_compound_assignment() {
    let source = r#"
@group(0) @binding(0) var<storage, read_write> out: array<f32>;

@compute @workgroup_size(1, 1, 1)
fn clamp(@builtin(global_invocation_id) gid: vec3<u32>) {
    var value = 1.0;
    value += 2.0;
    if (value > 2.0) {
        value = 4.0;
    }
    out[gid.x] = value;
}
"#;
    let lowered = lower_wgsl(source).expect("valid WGSL");
    let mut interpreter = CpuInterpreter::new();
    interpreter.with_buffer(0, vec![Value::F32(0.0); 1]);
    interpreter.run_parallel(&lowered.program, 1).expect("CPU execution");
    assert_eq!(interpreter.buffer(0), Some(vec![Value::F32(4.0)].as_slice()));
}

#[test]
fn rejects_module_without_compute_entry() {
    let error = lower_wgsl("fn main() {}").expect_err("a module without a compute entry must fail");
    assert!(matches!(error, FrontendError::Unsupported(message) if message.contains("@compute")));
}

#[test]
fn rejects_uniform_bindings() {
    let source = r#"
@group(0) @binding(0) var<uniform> params: vec3<f32>;
@compute @workgroup_size(1, 1, 1)
fn main() {}
"#;
    let error = lower_wgsl(source).expect_err("uniform bindings are not lowered yet");
    assert!(matches!(error, FrontendError::Unsupported(message) if message.contains("address space")));
}

#[test]
fn rejects_assignment_to_immutable_binding() {
    let source = r#"
@group(0) @binding(0) var<storage, read_write> out: array<f32>;

@compute @workgroup_size(1, 1, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let value = 1.0;
    value = 2.0;
    out[gid.x] = value;
}
"#;
    let error = lower_wgsl(source).expect_err("assigning to `let` must fail");
    assert!(matches!(error, FrontendError::Unsupported(message) if message.contains("immutable")));
}

#[test]
fn rejects_non_literal_loop_bounds() {
    let source = r#"
@group(0) @binding(0) var<storage, read_write> out: array<f32>;

@compute @workgroup_size(1, 1, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    var n = 4u;
    for (var i = 0u; i < n; i = i + 1u) {
        out[gid.x] = 1.0;
    }
}
"#;
    let error = lower_wgsl(source).expect_err("dynamic loop bounds are not lowered");
    assert!(matches!(error, FrontendError::Unsupported(message) if message.contains("integer literal")));
}
