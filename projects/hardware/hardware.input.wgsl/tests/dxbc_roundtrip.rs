use hardware_emitter::emit_program_dxbc;
use hardware_input_wgsl::lower_wgsl;

/// The lowered WGSL program must survive the internal DXBC emitter and the
/// `data-binary-dxbc` structural validator: the Windows path never needs an external
/// shader compiler.
#[test]
fn lowered_wgsl_emits_structurally_valid_dxbc() {
    let source = r#"
@group(0) @binding(0) var<storage, read_write> out: array<f32>;

@compute @workgroup_size(4, 1, 1)
fn fill(@builtin(global_invocation_id) gid: vec3<u32>) {
    out[gid.x] = 1.5;
}
"#;
    let lowered = lower_wgsl(source).expect("valid WGSL");
    let bytes = emit_program_dxbc(&lowered.program, &lowered.entry).expect("DXBC emission");
    assert_eq!(&bytes[0..4], b"DXBC");
    data_binary_dxbc::validate_dxbc_bytes(&bytes).expect("structurally valid DXBC");
}
