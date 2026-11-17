use hardware_frontend::FrontendError;
use hardware_input_hlsl::lower_hlsl;
use hardware_program::{CpuInterpreter, ScalarTy, Ty, Value};

fn fill_source() -> &'static str {
    r#"
RWStructuredBuffer<float> OutBuf : register(u0);

[numthreads(4, 1, 1)]
void fill(uint3 gid : SV_DispatchThreadID) {
    OutBuf[gid.x] = 1.5;
}
"#
}

#[test]
fn lowers_compute_fill_and_runs_on_cpu() {
    let lowered = lower_hlsl(fill_source()).expect("valid HLSL");
    assert_eq!(lowered.entry, "fill");
    assert_eq!(lowered.resources.len(), 1);

    let binding = &lowered.resources[0];
    assert_eq!(binding.name, "OutBuf");
    assert_eq!(binding.group, 0);
    assert_eq!(binding.binding, 0);
    assert_eq!(binding.element, Ty::Scalar(ScalarTy::F32));

    let mut interpreter = CpuInterpreter::new();
    interpreter.with_buffer(0, vec![Value::F32(0.0); 4]);
    interpreter.run_parallel(&lowered.program, 4).expect("CPU execution");
    assert_eq!(interpreter.buffer(0), Some(vec![Value::F32(1.5); 4].as_slice()));
}

#[test]
fn lowers_for_loop_with_postfix_increment() {
    let source = r#"
StructuredBuffer<float> values : register(t0);
RWStructuredBuffer<float> results : register(u0);

[numthreads(2, 1, 1)]
void sum(uint3 gid : SV_DispatchThreadID) {
    float acc = 0.0;
    for (uint k = 0u; k < 4u; k++) {
        acc += values[k];
    }
    results[gid.x] = acc;
}
"#;
    let lowered = lower_hlsl(source).expect("valid HLSL");
    assert_eq!(lowered.resources.len(), 2);
    let mut interpreter = CpuInterpreter::new();
    interpreter.with_buffer(0, vec![Value::F32(1.0), Value::F32(2.0), Value::F32(3.0), Value::F32(4.0)]);
    interpreter.with_buffer(1, vec![Value::F32(0.0); 2]);
    interpreter.run_parallel(&lowered.program, 2).expect("CPU execution");
    assert_eq!(interpreter.buffer(1), Some(vec![Value::F32(10.0); 2].as_slice()));
}

#[test]
fn lowers_uniform_block_into_slots_and_runs_on_cpu() {
    let source = r#"
struct Params {
    float scale;
};

uniform Params params : register(b0);
RWStructuredBuffer<float> OutBuf : register(u0);

[numthreads(1, 1, 1)]
void main(uint3 gid : SV_DispatchThreadID) {
    OutBuf[gid.x] = params.scale * 2.0;
}
"#;
    let lowered = lower_hlsl(source).expect("valid HLSL");
    assert_eq!(lowered.uniforms.len(), 1);

    let uniform = &lowered.uniforms[0];
    assert_eq!(uniform.name, "params");
    assert_eq!(uniform.group, 0);
    assert_eq!(uniform.binding, 0);
    assert_eq!(uniform.slot, 0);
    assert_eq!(uniform.fields, vec![("scale".to_owned(), Ty::Scalar(ScalarTy::F32))]);

    let mut interpreter = CpuInterpreter::new();
    interpreter.with_uniform(0, Value::F32(3.0));
    interpreter.with_buffer(0, vec![Value::F32(0.0); 1]);
    interpreter.run_parallel(&lowered.program, 1).expect("CPU execution");
    assert_eq!(interpreter.buffer(0), Some(vec![Value::F32(6.0)].as_slice()));
}

#[test]
fn lowers_cbuffer_block_into_uniform_slots_and_runs_on_cpu() {
    let source = r#"
cbuffer Params : register(b0)
{
    float scale;
    float bias;
};

RWStructuredBuffer<float> OutBuf : register(u0);

[numthreads(1, 1, 1)]
void main(uint3 gid : SV_DispatchThreadID) {
    OutBuf[gid.x] = Params.scale * 2.0 + Params.bias;
}
"#;
    let lowered = lower_hlsl(source).expect("valid HLSL");
    assert_eq!(lowered.uniforms.len(), 1);

    let uniform = &lowered.uniforms[0];
    assert_eq!(uniform.name, "Params");
    assert_eq!(uniform.binding, 0);
    assert_eq!(uniform.fields, vec![("scale".to_owned(), Ty::Scalar(ScalarTy::F32)), ("bias".to_owned(), Ty::Scalar(ScalarTy::F32)),]);

    let mut interpreter = CpuInterpreter::new();
    interpreter.with_uniform(0, Value::F32(3.0));
    interpreter.with_uniform(1, Value::F32(0.5));
    interpreter.with_buffer(0, vec![Value::F32(0.0); 1]);
    interpreter.run_parallel(&lowered.program, 1).expect("CPU execution");
    assert_eq!(interpreter.buffer(0), Some(vec![Value::F32(6.5)].as_slice()));
}

#[test]
fn rejects_unknown_uniform_field() {
    let source = r#"
struct Params {
    float scale;
};

uniform Params params : register(b0);
RWStructuredBuffer<float> OutBuf : register(u0);

[numthreads(1, 1, 1)]
void main(uint3 gid : SV_DispatchThreadID) {
    OutBuf[gid.x] = params.missing;
}
"#;
    let error = lower_hlsl(source).expect_err("an unknown uniform field must fail");
    assert!(matches!(error, FrontendError::Unsupported(message) if message.contains("missing")));
}

#[test]
fn rejects_byte_address_buffers() {
    let source = r#"
RWByteAddressBuffer OutBuf : register(u0);

[numthreads(1, 1, 1)]
void fill(uint3 gid : SV_DispatchThreadID) {
    OutBuf.Store(gid.x * 4, 1u);
}
"#;
    let error = lower_hlsl(source).expect_err("byte-address buffers are not lowered yet");
    assert!(error.to_string().contains("byte-address"));
}
