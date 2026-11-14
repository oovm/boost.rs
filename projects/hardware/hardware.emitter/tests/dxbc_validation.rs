use hardware_emitter::{emit_dxil, emit_raster_tri_dxbc, emit_vertex_raster_dxbc};
use hardware_types::ShaderProgram;

#[test]
fn emitted_compute_shaders_pass_structural_validation() {
    for program in [ShaderProgram::dispatch_fill("ucf_fill", 1.0), ShaderProgram::matmul("ucf_matmul")] {
        let bytes = emit_dxil(&program).expect("DXBC emission");
        assert_eq!(&bytes[0..4], b"DXBC");
        acorn_dxbc::validate_dxbc_bytes(&bytes).expect("structurally valid DXBC");
    }
}

#[test]
fn emitted_raster_triangle_passes_structural_validation() {
    let triangle = emit_raster_tri_dxbc().expect("DXBC emission");
    for bytes in [&triangle.vs, &triangle.ps] {
        assert_eq!(&bytes[0..4], b"DXBC");
        acorn_dxbc::validate_dxbc_bytes(bytes).expect("structurally valid DXBC");
    }
}

#[test]
fn emitted_vertex_raster_passes_structural_validation() {
    let vertex = emit_vertex_raster_dxbc().expect("DXBC emission");
    for bytes in [&vertex.vs, &vertex.ps] {
        assert_eq!(&bytes[0..4], b"DXBC");
        acorn_dxbc::validate_dxbc_bytes(bytes).expect("structurally valid DXBC");
    }
}
