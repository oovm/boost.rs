use hardware_types::{ShaderOp, ShaderProgram};

use crate::dxbc::{
    BoundBuffer, BufferRegister, Chunk, ConstantBuffer, Index, Opcode, Operand, Reflection, Register, Select, ShaderWriter, SignatureElement,
    Stage, TEST_NONZERO, Variable, container, reflection_chunk, signature_chunk,
};

/// Component indices of a vector operand.
const X: u8 = 0;
const Y: u8 = 1;
const Z: u8 = 2;
const W: u8 = 3;

/// Write mask selecting one component.
const fn one(component: u8) -> u8 {
    1 << component
}

/// Write mask selecting two components.
const fn two(first: u8, second: u8) -> u8 {
    (1 << first) | (1 << second)
}

/// Write mask selecting all four components.
const ALL: u8 = 0b1111;

/// `D3D_NAME` values referenced by the shaders below.
const NAME_POSITION: u32 = 1;
const NAME_VERTEX_ID: u32 = 6;

/// Signature component types (`D3D_REGISTER_COMPONENT_TYPE`).
const COMPONENT_UINT: u32 = 1;
const COMPONENT_FLOAT: u32 = 3;

/// VS + PS DXBC for the Raster triangle thin gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RasterTriDxbc {
    /// Vertex shader bytecode (`vs_5_0`).
    pub vs: Vec<u8>,
    /// Pixel shader bytecode (`ps_5_0`).
    pub ps: Vec<u8>,
}

/// VS + PS DXBC for a vertex-buffer raster pass.
///
/// The pair expects one input layout shared with the backend: `POSITION`
/// (`R32G32_FLOAT`, slot 0, offset 0) followed by `COLOR`
/// (`R32G32B32A32_FLOAT`, slot 0, offset 8). Positions are given in pixels of
/// the render target whose size is supplied as two 32-bit root constants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RasterVertexDxbc {
    /// Vertex shader bytecode (`vs_5_0`).
    pub vs: Vec<u8>,
    /// Pixel shader bytecode (`ps_5_0`).
    pub ps: Vec<u8>,
}

/// Emit DXBC compute bytecode (`cs_5_1`) for the given UCF program.
///
/// The module is encoded directly as DXBC container chunks; no shader source is
/// produced and no graphics compiler is invoked.
pub fn emit_dxil(program: &ShaderProgram) -> Result<Vec<u8>, String> {
    for op in &program.ops {
        match op {
            ShaderOp::MemCopy => continue,
            ShaderOp::IotaFill { value } => return Ok(fill_module(&program.entry, *value)),
            ShaderOp::MatMul => return Ok(matmul_module(&program.entry)),
            ShaderOp::Rgba8Denoise => {
                return Err("RGBA8 denoise is CUDA-only in this thin gate".into());
            }
            ShaderOp::Attention => {
                return Err("Attention HSACO/PTX path is not emitted as DXBC".into());
            }
        }
    }
    Err("program has no emittable DXBC ops".into())
}

/// Emit a hard-coded NDC triangle VS/PS pair (yellow solid) for Raster `draw=tri`.
pub fn emit_raster_tri_dxbc() -> Result<RasterTriDxbc, String> {
    Ok(RasterTriDxbc { vs: triangle_vertex_module(), ps: triangle_pixel_module() })
}

/// Emit a vertex-buffer triangle VS/PS pair (`vs_5_0` + `ps_5_0`).
///
/// The vertex shader reads `POSITION` and `COLOR` through the input assembler,
/// converts pixel coordinates to clip space with the viewport size taken from
/// two 32-bit root constants, and forwards the colour to the pixel shader, which
/// writes the interpolated colour unchanged.
pub fn emit_vertex_raster_dxbc() -> Result<RasterVertexDxbc, String> {
    Ok(RasterVertexDxbc { vs: vertex_buffer_vertex_module(), ps: vertex_buffer_pixel_module() })
}

/// `out[tid] = value` for `tid < count`, dispatched over 64-thread groups.
fn fill_module(entry: &str, value: f32) -> Vec<u8> {
    let mut shader = ShaderWriter::new(Stage::Compute, 1);
    shader.refactoring_allowed();
    shader.dcl_constant_buffer(0, 1);
    shader.dcl_uav_raw(0);
    shader.dcl_thread_id();
    shader.dcl_temps(1);
    shader.dcl_thread_group(64, 1, 1);

    let count = Operand::constant_buffer(0, 0, Select::Scalar(X));
    let thread = Operand::thread_id(Select::Scalar(X));
    // if (tid >= count) return;
    shader.op(Opcode::Uge, 0, &[Operand::temp(0, Select::Mask(one(X))), thread.clone(), count]);
    shader.op(Opcode::IfNz, TEST_NONZERO, &[Operand::temp(0, Select::Scalar(X))]);
    shader.op(Opcode::Ret, 0, &[]);
    shader.op(Opcode::EndIf, 0, &[]);

    // out[tid] = value, addressed by the byte offset `tid * 4`.
    shader.op(Opcode::IShl, 0, &[Operand::temp(0, Select::Mask(one(X))), thread, Operand::immediate(&[2])]);
    shader.op(
        Opcode::StoreRaw,
        0,
        &[Operand::bound(Register::Uav, 0, Select::Mask(one(X))), Operand::temp(0, Select::Scalar(X)), Operand::immediate(&[value.to_bits()])],
    );
    shader.op(Opcode::Ret, 0, &[]);

    let reflection = Reflection {
        stage: Stage::Compute,
        minor: 1,
        creator: creator("cs_5_1", entry),
        constant_buffers: vec![ConstantBuffer {
            name: "UcfFillParams".into(),
            size: 16,
            variables: vec![
                Variable { name: "count".into(), offset: 0, used: true },
                Variable { name: "_pad0".into(), offset: 4, used: false },
                Variable { name: "_pad1".into(), offset: 8, used: false },
                Variable { name: "_pad2".into(), offset: 12, used: false },
            ],
        }],
        buffers: vec![BoundBuffer { name: "OutBuf".into(), register: BufferRegister::Uav, slot: 0 }],
    };
    assemble(shader, reflection, &[], &[])
}

/// Row-major `C[m, n] = A[m, k] * B[k, n]`, one output element per thread.
fn matmul_module(entry: &str) -> Vec<u8> {
    let mut shader = ShaderWriter::new(Stage::Compute, 1);
    shader.refactoring_allowed();
    shader.dcl_constant_buffer(0, 1);
    shader.dcl_resource_raw(0);
    shader.dcl_resource_raw(1);
    shader.dcl_uav_raw(0);
    shader.dcl_thread_id();
    shader.dcl_temps(2);
    shader.dcl_thread_group(64, 1, 1);

    let m = Operand::constant_buffer(0, 0, Select::Scalar(X));
    let n = Operand::constant_buffer(0, 0, Select::Scalar(Y));
    let k = Operand::constant_buffer(0, 0, Select::Scalar(Z));
    let thread = Operand::thread_id(Select::Scalar(X));

    // r0.x = m * n
    shader.op(Opcode::IMul, 0, &[Operand::null(), Operand::temp(0, Select::Mask(one(X))), m, n.clone()]);
    // if (tid >= m * n) return;
    shader.op(Opcode::Uge, 0, &[Operand::temp(0, Select::Mask(one(X))), thread.clone(), Operand::temp(0, Select::Scalar(X))]);
    shader.op(Opcode::IfNz, TEST_NONZERO, &[Operand::temp(0, Select::Scalar(X))]);
    shader.op(Opcode::Ret, 0, &[]);
    shader.op(Opcode::EndIf, 0, &[]);

    // r0.x = tid / n (row), r1.x = tid % n (column).
    shader.op(Opcode::UDiv, 0, &[Operand::temp(0, Select::Mask(one(X))), Operand::temp(1, Select::Mask(one(X))), thread.clone(), n.clone()]);
    // r0.y accumulates the dot product, r0.z is the loop counter.
    shader.op(Opcode::Mov, 0, &[Operand::temp(0, Select::Mask(two(Y, Z))), Operand::immediate(&[0, 0, 0, 0])]);
    shader.op(Opcode::Loop, 0, &[]);
    // if (i >= k) break;
    shader.op(Opcode::Uge, 0, &[Operand::temp(0, Select::Mask(one(W))), Operand::temp(0, Select::Scalar(Z)), k.clone()]);
    shader.op(Opcode::BreakCNz, TEST_NONZERO, &[Operand::temp(0, Select::Scalar(W))]);

    // r0.w = A[(row * k + i) * 4]
    shader.op(
        Opcode::IMad,
        0,
        &[Operand::temp(0, Select::Mask(one(W))), Operand::temp(0, Select::Scalar(X)), k, Operand::temp(0, Select::Scalar(Z))],
    );
    shader.op(Opcode::IShl, 0, &[Operand::temp(0, Select::Mask(one(W))), Operand::temp(0, Select::Scalar(W)), Operand::immediate(&[2])]);
    shader.op(
        Opcode::LdRaw,
        0,
        &[Operand::temp(0, Select::Mask(one(W))), Operand::temp(0, Select::Scalar(W)), Operand::bound(Register::Resource, 0, Select::UNUSED)],
    );

    // r1.y = B[(i * n + column) * 4]
    shader.op(
        Opcode::IMad,
        0,
        &[Operand::temp(1, Select::Mask(one(Y))), Operand::temp(0, Select::Scalar(Z)), n, Operand::temp(1, Select::Scalar(X))],
    );
    shader.op(Opcode::IShl, 0, &[Operand::temp(1, Select::Mask(one(Y))), Operand::temp(1, Select::Scalar(Y)), Operand::immediate(&[2])]);
    shader.op(
        Opcode::LdRaw,
        0,
        &[Operand::temp(1, Select::Mask(one(Y))), Operand::temp(1, Select::Scalar(Y)), Operand::bound(Register::Resource, 1, Select::UNUSED)],
    );

    // sum += a * b, then advance the counter.
    shader.op(
        Opcode::Mad,
        0,
        &[
            Operand::temp(0, Select::Mask(one(Y))),
            Operand::temp(0, Select::Scalar(W)),
            Operand::temp(1, Select::Scalar(Y)),
            Operand::temp(0, Select::Scalar(Y)),
        ],
    );
    shader.op(Opcode::IAdd, 0, &[Operand::temp(0, Select::Mask(one(Z))), Operand::temp(0, Select::Scalar(Z)), Operand::immediate(&[1])]);
    shader.op(Opcode::EndLoop, 0, &[]);

    // out[tid] = sum
    shader.op(Opcode::IShl, 0, &[Operand::temp(0, Select::Mask(one(X))), thread, Operand::immediate(&[2])]);
    shader.op(
        Opcode::StoreRaw,
        0,
        &[Operand::bound(Register::Uav, 0, Select::Mask(one(X))), Operand::temp(0, Select::Scalar(X)), Operand::temp(0, Select::Scalar(Y))],
    );
    shader.op(Opcode::Ret, 0, &[]);

    let reflection = Reflection {
        stage: Stage::Compute,
        minor: 1,
        creator: creator("cs_5_1", entry),
        constant_buffers: vec![ConstantBuffer {
            name: "UcfMatMulParams".into(),
            size: 16,
            variables: vec![
                Variable { name: "m".into(), offset: 0, used: true },
                Variable { name: "n".into(), offset: 4, used: true },
                Variable { name: "k".into(), offset: 8, used: true },
                Variable { name: "_pad".into(), offset: 12, used: false },
            ],
        }],
        buffers: vec![
            BoundBuffer { name: "ABuf".into(), register: BufferRegister::Srv, slot: 0 },
            BoundBuffer { name: "BBuf".into(), register: BufferRegister::Srv, slot: 1 },
            BoundBuffer { name: "OutBuf".into(), register: BufferRegister::Uav, slot: 0 },
        ],
    };
    assemble(shader, reflection, &[], &[])
}

/// Triangle vertex shader: positions come from a table indexed by `SV_VertexID`.
fn triangle_vertex_module() -> Vec<u8> {
    const CORNERS: [(f32, f32); 3] = [(-0.8, -0.8), (0.8, -0.8), (0.0, 0.8)];
    const YELLOW: [u32; 4] = [1.0f32.to_bits(), 1.0f32.to_bits(), 0, 1.0f32.to_bits()];

    let mut shader = ShaderWriter::new(Stage::Vertex, 0);
    shader.refactoring_allowed();
    shader.dcl_input_sgv(0, one(X), NAME_VERTEX_ID);
    shader.dcl_output_sgv(0, ALL, NAME_POSITION);
    shader.dcl_output(1, ALL);
    shader.dcl_temps(1);
    shader.dcl_indexable_temp(0, CORNERS.len() as u32, 4);

    for (index, (x, y)) in CORNERS.iter().enumerate() {
        let operand = Operand::indexable_temp(Select::Mask(two(X, Y)), Index::Imm(index as u32));
        shader.op(Opcode::Mov, 0, &[operand, Operand::immediate(&[x.to_bits(), y.to_bits(), 0, 0])]);
    }

    // o0.xy = positions[vid], o0.zw = (0, 1), o1 = solid yellow.
    shader.op(Opcode::Mov, 0, &[Operand::temp(0, Select::Mask(one(X))), Operand::input(0, Select::Scalar(X))]);
    shader.op(Opcode::Mov, 0, &[Operand::output(0, Select::Mask(two(X, Y))), Operand::indexable_temp(Select::XYXX, Index::Relative(0, X))]);
    shader.op(Opcode::Mov, 0, &[Operand::output(0, Select::Mask(two(Z, W))), Operand::immediate(&[0, 0, 0, 1.0f32.to_bits()])]);
    shader.op(Opcode::Mov, 0, &[Operand::output(1, Select::Mask(ALL)), Operand::immediate(&YELLOW)]);
    shader.op(Opcode::Ret, 0, &[]);

    let inputs = [SignatureElement {
        name: "SV_VertexID",
        semantic_index: 0,
        system_value: NAME_VERTEX_ID,
        component_type: COMPONENT_UINT,
        register: 0,
        mask: one(X),
        read_mask: one(X),
    }];
    let outputs = [
        SignatureElement {
            name: "SV_Position",
            semantic_index: 0,
            system_value: NAME_POSITION,
            component_type: COMPONENT_FLOAT,
            register: 0,
            mask: ALL,
            read_mask: 0,
        },
        SignatureElement {
            name: "COLOR",
            semantic_index: 0,
            system_value: 0,
            component_type: COMPONENT_FLOAT,
            register: 1,
            mask: ALL,
            read_mask: 0,
        },
    ];
    let reflection = Reflection {
        stage: Stage::Vertex,
        minor: 0,
        creator: creator("vs_5_0", "raster_triangle"),
        constant_buffers: Vec::new(),
        buffers: Vec::new(),
    };
    assemble(shader, reflection, &inputs, &outputs)
}

/// Triangle pixel shader: forwards the interpolated vertex colour.
fn triangle_pixel_module() -> Vec<u8> {
    let mut shader = ShaderWriter::new(Stage::Pixel, 0);
    shader.refactoring_allowed();
    shader.dcl_input_ps_linear(1, ALL);
    shader.dcl_output(0, ALL);
    shader.op(Opcode::Mov, 0, &[Operand::output(0, Select::Mask(ALL)), Operand::input(1, Select::ALL)]);
    shader.op(Opcode::Ret, 0, &[]);

    let inputs = [
        SignatureElement {
            name: "SV_Position",
            semantic_index: 0,
            system_value: NAME_POSITION,
            component_type: COMPONENT_FLOAT,
            register: 0,
            mask: ALL,
            read_mask: 0,
        },
        SignatureElement {
            name: "COLOR",
            semantic_index: 0,
            system_value: 0,
            component_type: COMPONENT_FLOAT,
            register: 1,
            mask: ALL,
            read_mask: ALL,
        },
    ];
    let outputs = [SignatureElement {
        name: "SV_Target",
        semantic_index: 0,
        system_value: 0,
        component_type: COMPONENT_FLOAT,
        register: 0,
        mask: ALL,
        read_mask: 0,
    }];
    let reflection = Reflection {
        stage: Stage::Pixel,
        minor: 0,
        creator: creator("ps_5_0", "raster_triangle"),
        constant_buffers: Vec::new(),
        buffers: Vec::new(),
    };
    assemble(shader, reflection, &inputs, &outputs)
}

/// Vertex shader of the vertex-buffer path: pixels in, clip space out.
fn vertex_buffer_vertex_module() -> Vec<u8> {
    let mut shader = ShaderWriter::new(Stage::Vertex, 0);
    shader.refactoring_allowed();
    shader.dcl_constant_buffer(0, 1);
    shader.dcl_input(0, two(X, Y));
    shader.dcl_input(1, ALL);
    shader.dcl_output_sgv(0, ALL, NAME_POSITION);
    shader.dcl_output(1, ALL);
    shader.dcl_temps(1);

    // r0.x = position.x / width, r0.y = position.y / height.
    shader.op(
        Opcode::Div,
        0,
        &[Operand::temp(0, Select::Mask(one(X))), Operand::input(0, Select::Scalar(X)), Operand::constant_buffer(0, 0, Select::Scalar(X))],
    );
    shader.op(
        Opcode::Div,
        0,
        &[Operand::temp(0, Select::Mask(one(Y))), Operand::input(0, Select::Scalar(Y)), Operand::constant_buffer(0, 0, Select::Scalar(Y))],
    );
    // Clip space: x = 2t.x - 1, y = 1 - 2t.y (the target origin is top-left).
    shader.op(
        Opcode::Mad,
        0,
        &[
            Operand::temp(0, Select::Mask(two(X, Y))),
            Operand::temp(0, Select::XYXX),
            Operand::immediate(&[2.0f32.to_bits(), (-2.0f32).to_bits(), 0, 0]),
            Operand::immediate(&[(-1.0f32).to_bits(), 1.0f32.to_bits(), 0, 0]),
        ],
    );

    shader.op(Opcode::Mov, 0, &[Operand::output(0, Select::Mask(two(X, Y))), Operand::temp(0, Select::XYXX)]);
    shader.op(Opcode::Mov, 0, &[Operand::output(0, Select::Mask(two(Z, W))), Operand::immediate(&[0, 0, 0, 1.0f32.to_bits()])]);
    shader.op(Opcode::Mov, 0, &[Operand::output(1, Select::Mask(ALL)), Operand::input(1, Select::ALL)]);
    shader.op(Opcode::Ret, 0, &[]);

    let inputs = [
        SignatureElement {
            name: "POSITION",
            semantic_index: 0,
            system_value: 0,
            component_type: COMPONENT_FLOAT,
            register: 0,
            mask: two(X, Y),
            read_mask: two(X, Y),
        },
        SignatureElement {
            name: "COLOR",
            semantic_index: 0,
            system_value: 0,
            component_type: COMPONENT_FLOAT,
            register: 1,
            mask: ALL,
            read_mask: ALL,
        },
    ];
    let outputs = [
        SignatureElement {
            name: "SV_Position",
            semantic_index: 0,
            system_value: NAME_POSITION,
            component_type: COMPONENT_FLOAT,
            register: 0,
            mask: ALL,
            read_mask: 0,
        },
        SignatureElement {
            name: "COLOR",
            semantic_index: 0,
            system_value: 0,
            component_type: COMPONENT_FLOAT,
            register: 1,
            mask: ALL,
            read_mask: 0,
        },
    ];
    let reflection = Reflection {
        stage: Stage::Vertex,
        minor: 0,
        creator: creator("vs_5_0", "vertex_raster"),
        constant_buffers: vec![ConstantBuffer {
            name: "Viewport".into(),
            size: 16,
            variables: vec![
                Variable { name: "width".into(), offset: 0, used: true },
                Variable { name: "height".into(), offset: 4, used: true },
                Variable { name: "_pad0".into(), offset: 8, used: false },
                Variable { name: "_pad1".into(), offset: 12, used: false },
            ],
        }],
        buffers: Vec::new(),
    };
    assemble(shader, reflection, &inputs, &outputs)
}

/// Pixel shader of the vertex-buffer path: forwards the interpolated colour.
fn vertex_buffer_pixel_module() -> Vec<u8> {
    let mut shader = ShaderWriter::new(Stage::Pixel, 0);
    shader.refactoring_allowed();
    shader.dcl_input_ps_linear(1, ALL);
    shader.dcl_output(0, ALL);
    shader.op(Opcode::Mov, 0, &[Operand::output(0, Select::Mask(ALL)), Operand::input(1, Select::ALL)]);
    shader.op(Opcode::Ret, 0, &[]);

    let inputs = [
        SignatureElement {
            name: "SV_Position",
            semantic_index: 0,
            system_value: NAME_POSITION,
            component_type: COMPONENT_FLOAT,
            register: 0,
            mask: ALL,
            read_mask: 0,
        },
        SignatureElement {
            name: "COLOR",
            semantic_index: 0,
            system_value: 0,
            component_type: COMPONENT_FLOAT,
            register: 1,
            mask: ALL,
            read_mask: ALL,
        },
    ];
    let outputs = [SignatureElement {
        name: "SV_Target",
        semantic_index: 0,
        system_value: 0,
        component_type: COMPONENT_FLOAT,
        register: 0,
        mask: ALL,
        read_mask: 0,
    }];
    let reflection = Reflection {
        stage: Stage::Pixel,
        minor: 0,
        creator: creator("ps_5_0", "vertex_raster"),
        constant_buffers: Vec::new(),
        buffers: Vec::new(),
    };
    assemble(shader, reflection, &inputs, &outputs)
}

/// Creator string recorded in the module's reflection chunk.
fn creator(profile: &str, entry: &str) -> String {
    format!("hardware-emitter {profile} {entry}")
}

fn assemble(shader: ShaderWriter, reflection: Reflection, inputs: &[SignatureElement], outputs: &[SignatureElement]) -> Vec<u8> {
    container(&[
        Chunk::new(*b"RDEF", reflection_chunk(&reflection)),
        Chunk::new(*b"ISGN", signature_chunk(inputs)),
        Chunk::new(*b"OSGN", signature_chunk(outputs)),
        Chunk::new(*b"SHEX", shader.finish()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(blob: &[u8], fourcc: &[u8; 4]) -> Option<Vec<u8>> {
        let count = u32::from_le_bytes(blob[28..32].try_into().unwrap()) as usize;
        for index in 0..count {
            let field = 32 + index * 4;
            let offset = u32::from_le_bytes(blob[field..field + 4].try_into().unwrap()) as usize;
            if &blob[offset..offset + 4] == fourcc {
                let size = u32::from_le_bytes(blob[offset + 4..offset + 8].try_into().unwrap()) as usize;
                return Some(blob[offset + 8..offset + 8 + size].to_vec());
            }
        }
        None
    }

    #[test]
    fn emit_fill_and_matmul_dxbc() {
        let fill = emit_dxil(&ShaderProgram::dispatch_fill("ucf_fill", 1.0)).expect("fill");
        assert_eq!(&fill[0..4], b"DXBC");
        assert_eq!(u32::from_le_bytes(fill[24..28].try_into().unwrap()) as usize, fill.len());
        let program = chunk(&fill, b"SHEX").expect("fill shex");
        let version = u32::from_le_bytes(program[0..4].try_into().unwrap());
        assert_eq!(version, 0x0005_0051, "cs_5_1");
        assert_eq!(u32::from_le_bytes(program[4..8].try_into().unwrap()) as usize, program.len() / 4);

        let matmul = emit_dxil(&ShaderProgram::matmul("ucf_matmul")).expect("matmul");
        assert_eq!(&matmul[0..4], b"DXBC");
        assert!(chunk(&matmul, b"SHEX").is_some());
    }

    #[test]
    fn emit_raster_tri_vs_ps_dxbc() {
        let tri = emit_raster_tri_dxbc().expect("tri");
        assert_eq!(&tri.vs[0..4], b"DXBC");
        assert_eq!(&tri.ps[0..4], b"DXBC");
        let vs = chunk(&tri.vs, b"SHEX").expect("vs shex");
        let ps = chunk(&tri.ps, b"SHEX").expect("ps shex");
        assert_eq!(u32::from_le_bytes(vs[0..4].try_into().unwrap()), 0x0001_0050, "vs_5_0");
        assert_eq!(u32::from_le_bytes(ps[0..4].try_into().unwrap()), 0x0000_0050, "ps_5_0");
    }

    #[test]
    fn emit_vertex_raster_vs_ps_dxbc() {
        let vertex = emit_vertex_raster_dxbc().expect("vertex raster");
        assert_eq!(&vertex.vs[0..4], b"DXBC");
        assert_eq!(&vertex.ps[0..4], b"DXBC");
        let vs = chunk(&vertex.vs, b"SHEX").expect("vs shex");
        let ps = chunk(&vertex.ps, b"SHEX").expect("ps shex");
        assert_eq!(u32::from_le_bytes(vs[0..4].try_into().unwrap()), 0x0001_0050, "vs_5_0");
        assert_eq!(u32::from_le_bytes(ps[0..4].try_into().unwrap()), 0x0000_0050, "ps_5_0");
        let inputs = chunk(&vertex.vs, b"ISGN").expect("vs isgn");
        assert!(inputs.windows(8).any(|window| window == b"POSITION"));
        assert!(inputs.windows(5).any(|window| window == b"COLOR"));
    }

    #[test]
    fn unsupported_ops_report_errors() {
        assert!(emit_dxil(&ShaderProgram::rgba8_denoise("ucf_denoise")).is_err());
        assert!(emit_dxil(&ShaderProgram::attention("ucf_attention")).is_err());
        assert!(emit_dxil(&ShaderProgram::mem_copy("ucf_copy")).is_err());
    }
}
