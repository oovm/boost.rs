use hardware_emitter::emit_program_spirv;
use hardware_frontend::lower_wgsl;

/// The lowered WGSL program must survive the internal SPIR-V emitter and the
/// `acorn-spirv` structural validator.
#[test]
fn lowered_wgsl_emits_structurally_valid_spirv() {
    let source = r#"
@group(0) @binding(0) var<storage, read_write> out: array<f32>;

@compute @workgroup_size(4, 1, 1)
fn fill(@builtin(global_invocation_id) gid: vec3<u32>) {
    out[gid.x] = 1.5;
}
"#;
    let lowered = lower_wgsl(source).expect("valid WGSL");
    let bytes = emit_program_spirv(&lowered.program, &lowered.entry).expect("SPIR-V emission");
    assert_eq!(&bytes[0..4], &0x0723_0203u32.to_le_bytes());

    let summary = acorn_spirv::validate_spirv_bytes(&bytes).expect("structurally valid SPIR-V");
    assert!(summary.instruction_words > 0);
}
