//! 真实 GPU 上的重复性/一致性压测：同一批缓冲反复改写、提交、读回。

use std::collections::BTreeMap;

use hardware_backend_dx12::Dx12Backend;
use hardware_ir::{
    Access, DepEdge, DepKind, Domain, Graph, Objective, ParamValue, Priority, ResourceGraph, ResourceId, ResourceKind, ResourceNode, ShaderId,
    TaskGraph, TaskId, TaskKind, TaskNode,
};
use hardware_scheduler::Backend;
use hardware_types::ResourceId as Rid;

const M: usize = 64;
const K: usize = 128;
const N: usize = 96;

fn buffer(id: u64, floats: usize) -> ResourceNode {
    ResourceNode {
        id: ResourceId(id),
        kind: ResourceKind::Buffer,
        domain: Domain::Vram,
        access: Access::ReadWrite,
        byte_size: Some((floats * 4) as u64),
    }
}

fn task(id: u64, kind: TaskKind, params: BTreeMap<String, ParamValue>) -> TaskNode {
    TaskNode {
        id: TaskId(id),
        kind,
        shader: ShaderId(id),
        params,
        dispatch: Default::default(),
        objective: Objective::MaxThroughput,
        priority: Priority::Batch,
    }
}

fn param(key: &str, value: i64) -> (String, ParamValue) {
    (key.into(), ParamValue::I64(value))
}

fn graph() -> Graph {
    Graph {
        resources: ResourceGraph { nodes: vec![buffer(1, M * K), buffer(2, K * N), buffer(3, M * N), buffer(4, M * N)] },
        tasks: TaskGraph {
            nodes: vec![
                task(
                    100,
                    TaskKind::MatMul,
                    BTreeMap::from([
                        param("a", 1),
                        param("b", 2),
                        param("out", 3),
                        param("m", M as i64),
                        param("n", N as i64),
                        param("k", K as i64),
                    ]),
                ),
                task(101, TaskKind::Copy, BTreeMap::from([param("src", 3), param("dst", 4)])),
            ],
            edges: vec![DepEdge { from_task: Some(TaskId(100)), from_resource: None, to_task: TaskId(101), kind: DepKind::Execution }],
        },
    }
}

fn cpu_matmul(a: &[f32], b: &[f32]) -> Vec<f32> {
    let mut out = vec![0.0f32; M * N];
    for row in 0..M {
        for column in 0..N {
            let mut sum = 0.0f32;
            for inner in 0..K {
                sum += a[row * K + inner] * b[inner * N + column];
            }
            out[row * N + column] = sum;
        }
    }
    out
}

fn lcg(seed: &mut u64) -> f32 {
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    ((*seed >> 33) as f32 / (u32::MAX >> 1) as f32) * 2.0 - 1.0
}

#[test]
fn repeated_matmul_copy_matches_cpu_on_the_device() {
    let mut backend = match Dx12Backend::new() {
        Ok(backend) => backend,
        Err(error) => {
            eprintln!("skip dx12 stress (no usable D3D12 device): {error}");
            return;
        }
    };
    let graph = graph();
    backend.prepare(&graph).expect("prepare");

    let mut seed = 0x5eed_u64;
    for iteration in 0..10 {
        let a: Vec<f32> = (0..M * K).map(|_| lcg(&mut seed)).collect();
        let b: Vec<f32> = (0..K * N).map(|_| lcg(&mut seed)).collect();
        let expected = cpu_matmul(&a, &b);

        backend.write_f32(Rid(1), &a).expect("upload a");
        backend.write_f32(Rid(2), &b).expect("upload b");
        backend.run_prepared(&graph).expect("dispatch");

        let actual = backend.read_f32(Rid(4)).expect("read output");
        assert_eq!(actual.len(), expected.len());
        let worst = actual.iter().zip(&expected).map(|(left, right)| (left - right).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-2, "iteration {iteration}: max |gpu - cpu| = {worst}");
        assert!(actual.iter().all(|value| value.is_finite()), "iteration {iteration}: non-finite output");

        // 不重新上传再跑一次，结果必须完全一致（没有残留状态或竞争）。
        backend.run_prepared(&graph).expect("dispatch again");
        assert_eq!(backend.read_f32(Rid(4)).expect("read again"), actual, "iteration {iteration}: rerun drifted");
    }
}
