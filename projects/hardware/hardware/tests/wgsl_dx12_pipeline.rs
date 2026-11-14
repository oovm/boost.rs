//! End-to-end WGSL → UCF → `DXBC` → `Direct3D 12` execution.
//!
//! The shader text is consumed by the frontend only. Everything below that is
//! self-emitted bytecode and the backend's own driver calls, with no external
//! shader compiler in the chain.

use std::collections::BTreeMap;

use hardware_backend_dx12::Dx12Backend;
use hardware_input_wgsl::lower_wgsl;
use hardware_ir::{
    Access, Domain, Graph, ParamValue, Priority, ResourceGraph, ResourceId, ResourceKind, ResourceNode, ShaderId, TaskGraph, TaskId, TaskKind, TaskNode,
};
use hardware_program::program_hash;
use hardware_scheduler::Backend;
use hardware_types::{Dispatch, ResourceId as Rid};

const SOURCE: &str = r#"
struct Params {
    scale: f32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> src: array<f32>;
@group(0) @binding(2) var<storage, read_write> dst: array<f32>;

@compute @workgroup_size(4, 1, 1)
fn scale_in(@builtin(global_invocation_id) gid: vec3<u32>) {
    dst[gid.x] = src[gid.x] * params.scale;
}
"#;

fn buffer(id: u64, floats: usize) -> ResourceNode {
    ResourceNode {
        id: ResourceId(id),
        kind: ResourceKind::Buffer,
        domain: Domain::Vram,
        access: Access::ReadWrite,
        byte_size: Some((floats * 4) as u64),
    }
}

#[test]
fn lowered_wgsl_runs_on_direct3d_12() {
    let lowered = lower_wgsl(SOURCE).expect("valid WGSL");
    assert_eq!(lowered.resources.len(), 2, "src and dst storage buffers");
    assert_eq!(lowered.uniforms.len(), 1, "one uniform block");
    assert_eq!(lowered.uniforms[0].slot, 0);

    let src = 1 + lowered.resources[0].resource as u64;
    let dst = 1 + lowered.resources[1].resource as u64;
    let output = 100;
    let graph = Graph {
        resources: ResourceGraph { nodes: vec![buffer(src, 4), buffer(dst, 4), buffer(output, 4)] },
        tasks: TaskGraph {
            nodes: vec![TaskNode {
                id: TaskId(1),
                kind: TaskKind::Dispatch,
                shader: ShaderId(1),
                params: BTreeMap::from([
                    ("program".to_string(), ParamValue::Program {
                        version: 1,
                        hash: program_hash(&lowered.program),
                        body: serde_json::to_string(&lowered.program).expect("serialize program"),
                    }),
                    ("output".to_string(), ParamValue::I64(output as i64)),
                    (format!("buffer_{}", lowered.resources[0].resource), ParamValue::I64(src as i64)),
                    (format!("buffer_{}", lowered.resources[1].resource), ParamValue::I64(dst as i64)),
                    ("uniform_0".to_string(), ParamValue::F64(2.5)),
                ]),
                dispatch: Dispatch::Explicit { x: 4, y: 1, z: 1 },
                objective: hardware_ir::Objective::MaxThroughput,
                priority: Priority::Batch,
            }],
            edges: Vec::new(),
        },
    };

    let mut dx12 = match Dx12Backend::new() {
        Ok(backend) => backend,
        Err(error) => {
            eprintln!("skip WGSL DX12 pipeline (no usable D3D12 device): {error}");
            return;
        }
    };
    dx12.prepare(&graph).expect("prepare");
    dx12.write_f32(Rid(src), &[1.0, 2.0, 3.0, 4.0]).expect("seed");
    dx12.run_prepared(&graph).expect("dispatch");
    assert_eq!(dx12.read_f32(Rid(dst)).expect("read dst"), vec![2.5, 5.0, 7.5, 10.0]);
}
