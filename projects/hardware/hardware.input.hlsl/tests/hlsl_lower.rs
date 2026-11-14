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
