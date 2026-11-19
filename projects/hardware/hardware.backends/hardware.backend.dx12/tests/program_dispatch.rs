//! Direct3D 12 dispatch of serialized generic UCF programs.
//!
//! These tests need a usable D3D12 adapter and skip with a message otherwise,
//! matching the other device tests in this crate.

use std::collections::BTreeMap;

use hardware_backend_dx12::Dx12Backend;
use hardware_ir::{
    Access, Domain, Graph, ParamValue, Priority, ResourceGraph, ResourceId, ResourceKind, ResourceNode, ShaderId, TaskGraph, TaskId, TaskKind,
    TaskNode,
};
use hardware_program::{BinOp, Expr, Function, Program, Stmt, Ty, program_hash};
use hardware_scheduler::Backend;
use hardware_types::{Dispatch, ResourceId as Rid};

fn buffer(id: u64, floats: usize) -> ResourceNode {
    ResourceNode {
        id: ResourceId(id),
        kind: ResourceKind::Buffer,
        domain: Domain::Vram,
        access: Access::ReadWrite,
        byte_size: Some((floats * 4) as u64),
    }
}

fn dispatch_task(id: u64, program: &Program, params: BTreeMap<String, ParamValue>) -> TaskNode {
    let mut params = params;
    params.insert(
        "program".into(),
        ParamValue::Program { version: 1, hash: program_hash(program), body: serde_json::to_string(program).expect("serialize program") },
    );
    TaskNode {
        id: TaskId(id),
        kind: TaskKind::Dispatch,
        shader: ShaderId(id),
        params,
        dispatch: Dispatch::Explicit { x: 4, y: 1, z: 1 },
        objective: hardware_ir::Objective::MaxThroughput,
        priority: Priority::Batch,
    }
}

fn program(body: Vec<Stmt>) -> Program {
    Program {
        version: 1,
        functions: vec![Function { name: "main".into(), params: Vec::new(), locals: Vec::new(), body, ret: Ty::f32() }],
        entry: 0,
    }
}

fn open() -> Option<Dx12Backend> {
    match Dx12Backend::new() {
        Ok(backend) => Some(backend),
        Err(error) => {
            eprintln!("skip dx12 program dispatch (no usable D3D12 device): {error}");
            None
        }
    }
}

#[test]
fn dispatches_uniform_scaled_buffer_reads() {
    let program = program(vec![Stmt::Return(Expr::BinOp {
        op: BinOp::Mul,
        lhs: Box::new(Expr::LoadUniform(0)),
        rhs: Box::new(Expr::LoadBuffer { resource: 0, index: Box::new(Expr::GlobalId) }),
    })]);
    let graph = Graph {
        resources: ResourceGraph { nodes: vec![buffer(1, 4), buffer(2, 4)] },
        tasks: TaskGraph {
            nodes: vec![dispatch_task(
                10,
                &program,
                BTreeMap::from([
                    ("output".to_string(), ParamValue::I64(2)),
                    ("buffer_0".to_string(), ParamValue::I64(1)),
                    ("uniform_0".to_string(), ParamValue::F64(2.0)),
                ]),
            )],
            edges: Vec::new(),
        },
    };

    let Some(mut dx12) = open()
    else {
        return;
    };
    dx12.prepare(&graph).expect("prepare");
    dx12.write_f32(Rid(1), &[1.0, 2.0, 3.0, 4.0]).expect("seed");
    dx12.run_prepared(&graph).expect("dispatch");
    let actual = dx12.read_f32(Rid(2)).expect("read");
    assert_eq!(actual, vec![2.0, 4.0, 6.0, 8.0]);
}

#[test]
fn dispatches_vector_math_program() {
    use hardware_program::{StructuredFunctionBuilder, UnOp};

    let builder = StructuredFunctionBuilder::new("vector_math");
    let a = Expr::Vec3Compose { x: Box::new(Expr::ConstF32(1.0)), y: Box::new(Expr::ConstF32(2.0)), z: Box::new(Expr::ConstF32(2.0)) };
    let b = Expr::Vec3Compose { x: Box::new(Expr::ConstF32(0.0)), y: Box::new(Expr::ConstF32(3.0)), z: Box::new(Expr::ConstF32(0.0)) };
    let unit = builder.unary(UnOp::Normalize, a.clone());
    let dot = builder.binary(BinOp::Dot, unit, b);
    let length = builder.unary(UnOp::Length, a);
    let remainder = builder.binary(BinOp::Mod, Expr::ConstF32(5.5), Expr::ConstF32(2.0));
    let sum = builder.binary(BinOp::Add, dot, length);
    let result = builder.binary(BinOp::Add, sum, remainder);
    let function = builder.finish(Ty::f32(), result);
    let program = Program { version: 1, functions: vec![function], entry: 0 };

    let graph = Graph {
        resources: ResourceGraph { nodes: vec![buffer(2, 4)] },
        tasks: TaskGraph {
            nodes: vec![dispatch_task(13, &program, BTreeMap::from([("output".to_string(), ParamValue::I64(2))]))],
            edges: Vec::new(),
        },
    };

    let Some(mut dx12) = open()
    else {
        return;
    };
    dx12.prepare(&graph).expect("prepare");
    dx12.run_prepared(&graph).expect("dispatch");
    let actual = dx12.read_f32(Rid(2)).expect("read");
    assert_eq!(actual, vec![6.5; 4]);
}

#[test]
fn dispatches_comparison_and_select() {
    let cases = [(BinOp::Ge, [0.0f32, 0.0, 1.0, 1.0]), (BinOp::Gt, [0.0f32, 0.0, 0.0, 1.0]), (BinOp::Eq, [0.0f32, 0.0, 1.0, 0.0])];
    for (op, expected) in cases {
        let entry = program(vec![Stmt::Return(Expr::Select {
            cond: Box::new(Expr::BinOp { op, lhs: Box::new(Expr::GlobalId), rhs: Box::new(Expr::ConstU32(2)) }),
            then_: Box::new(Expr::ConstF32(1.0)),
            else_: Box::new(Expr::ConstF32(0.0)),
        })]);
        let graph = Graph {
            resources: ResourceGraph { nodes: vec![buffer(2, 4)] },
            tasks: TaskGraph {
                nodes: vec![dispatch_task(16, &entry, BTreeMap::from([("output".to_string(), ParamValue::I64(2))]))],
                edges: Vec::new(),
            },
        };

        let Some(mut dx12) = open()
        else {
            return;
        };
        dx12.prepare(&graph).expect("prepare");
        dx12.run_prepared(&graph).expect("dispatch");
        assert_eq!(dx12.read_f32(Rid(2)).expect("read"), expected.to_vec(), "{op:?}");
    }
}

#[test]
fn reuses_cached_pipelines_for_repeated_programs() {
    let program =
        program(vec![Stmt::Return(Expr::BinOp { op: BinOp::Add, lhs: Box::new(Expr::LoadUniform(0)), rhs: Box::new(Expr::ConstF32(1.0)) })]);
    let graph = Graph {
        resources: ResourceGraph { nodes: vec![buffer(2, 4)] },
        tasks: TaskGraph {
            nodes: vec![dispatch_task(
                18,
                &program,
                BTreeMap::from([("output".to_string(), ParamValue::I64(2)), ("uniform_0".to_string(), ParamValue::F64(0.5))]),
            )],
            edges: Vec::new(),
        },
    };

    let Some(mut dx12) = open()
    else {
        return;
    };
    dx12.prepare(&graph).expect("prepare");
    dx12.run_prepared(&graph).expect("first dispatch");
    assert_eq!(dx12.program_cache_stats(), hardware_backend_dx12::ProgramCacheStats { hits: 0, misses: 1, entries: 1 });

    dx12.run_prepared(&graph).expect("second dispatch");
    assert_eq!(dx12.program_cache_stats(), hardware_backend_dx12::ProgramCacheStats { hits: 1, misses: 1, entries: 1 });
    assert_eq!(dx12.read_f32(Rid(2)).expect("read"), vec![1.5; 4]);

    dx12.clear_program_cache();
    assert_eq!(dx12.program_cache_stats().entries, 0);
}

#[test]
fn dispatches_polar_math() {
    use hardware_program::UnOp;

    let builder = hardware_program::StructuredFunctionBuilder::new("polar_math");
    let atan2 = builder.binary(BinOp::Atan2, Expr::ConstF32(1.0), Expr::ConstF32(1.0));
    let atan = builder.unary(UnOp::Atan, Expr::ConstF32(1.0));
    let sum = builder.binary(BinOp::Add, atan2, atan);
    let entry = Program { version: 1, functions: vec![builder.finish(Ty::f32(), sum)], entry: 0 };

    let graph = Graph {
        resources: ResourceGraph { nodes: vec![buffer(2, 4)] },
        tasks: TaskGraph {
            nodes: vec![dispatch_task(19, &entry, BTreeMap::from([("output".to_string(), ParamValue::I64(2))]))],
            edges: Vec::new(),
        },
    };

    let Some(mut dx12) = open()
    else {
        return;
    };
    dx12.prepare(&graph).expect("prepare");
    dx12.run_prepared(&graph).expect("dispatch");
    // atan2(1, 1) = atan(1) = π/4, so the sum is π/2.
    let actual = dx12.read_f32(Rid(2)).expect("read");
    for value in actual {
        assert!((value - std::f32::consts::FRAC_PI_2).abs() < 1e-3, "expected π/2, got {value}");
    }
}

#[test]
fn dispatches_bounded_loop_accumulation() {
    let entry = program(vec![
        Stmt::Let { id: 0, ty: Ty::f32(), init: Expr::ConstF32(0.0) },
        Stmt::For {
            iter: 1,
            limit: 4,
            body: vec![Stmt::StoreLocal {
                id: 0,
                value: Expr::BinOp { op: BinOp::Add, lhs: Box::new(Expr::LoadLocal(0)), rhs: Box::new(Expr::ConstF32(1.0)) },
            }],
        },
        Stmt::Return(Expr::LoadLocal(0)),
    ]);

    let graph = Graph {
        resources: ResourceGraph { nodes: vec![buffer(2, 4)] },
        tasks: TaskGraph {
            nodes: vec![dispatch_task(15, &entry, BTreeMap::from([("output".to_string(), ParamValue::I64(2))]))],
            edges: Vec::new(),
        },
    };

    let Some(mut dx12) = open()
    else {
        return;
    };
    dx12.prepare(&graph).expect("prepare");
    dx12.run_prepared(&graph).expect("dispatch");
    assert_eq!(dx12.read_f32(Rid(2)).expect("read"), vec![4.0; 4]);
}

#[test]
fn dispatches_loop_with_continue() {
    let entry = program(vec![
        Stmt::Let { id: 0, ty: Ty::f32(), init: Expr::ConstF32(0.0) },
        Stmt::For {
            iter: 1,
            limit: 4,
            body: vec![
                Stmt::If {
                    cond: Expr::BinOp { op: BinOp::Eq, lhs: Box::new(Expr::LoadLocal(1)), rhs: Box::new(Expr::ConstU32(1)) },
                    then_body: vec![Stmt::Continue],
                    else_body: Vec::new(),
                },
                Stmt::StoreLocal {
                    id: 0,
                    value: Expr::BinOp { op: BinOp::Add, lhs: Box::new(Expr::LoadLocal(0)), rhs: Box::new(Expr::ConstF32(1.0)) },
                },
            ],
        },
        Stmt::Return(Expr::LoadLocal(0)),
    ]);

    let graph = Graph {
        resources: ResourceGraph { nodes: vec![buffer(2, 4)] },
        tasks: TaskGraph {
            nodes: vec![dispatch_task(17, &entry, BTreeMap::from([("output".to_string(), ParamValue::I64(2))]))],
            edges: Vec::new(),
        },
    };

    let Some(mut dx12) = open()
    else {
        return;
    };
    dx12.prepare(&graph).expect("prepare");
    dx12.run_prepared(&graph).expect("dispatch");
    // One of four iterations is skipped, so three increments remain.
    assert_eq!(dx12.read_f32(Rid(2)).expect("read"), vec![3.0; 4]);
}

#[test]
fn dispatches_loop_with_break() {
    let entry = program(vec![
        Stmt::Let { id: 0, ty: Ty::f32(), init: Expr::ConstF32(0.0) },
        Stmt::For {
            iter: 1,
            limit: 4,
            body: vec![
                Stmt::If {
                    cond: Expr::BinOp { op: BinOp::Eq, lhs: Box::new(Expr::LoadLocal(1)), rhs: Box::new(Expr::ConstU32(2)) },
                    then_body: vec![Stmt::Break],
                    else_body: Vec::new(),
                },
                Stmt::StoreLocal {
                    id: 0,
                    value: Expr::BinOp { op: BinOp::Add, lhs: Box::new(Expr::LoadLocal(0)), rhs: Box::new(Expr::ConstF32(1.0)) },
                },
            ],
        },
        Stmt::Return(Expr::LoadLocal(0)),
    ]);

    let graph = Graph {
        resources: ResourceGraph { nodes: vec![buffer(2, 4)] },
        tasks: TaskGraph {
            nodes: vec![dispatch_task(14, &entry, BTreeMap::from([("output".to_string(), ParamValue::I64(2))]))],
            edges: Vec::new(),
        },
    };

    let Some(mut dx12) = open()
    else {
        return;
    };
    dx12.prepare(&graph).expect("prepare");
    dx12.run_prepared(&graph).expect("dispatch");
    // The loop stops when the counter reaches two, so two increments are kept.
    assert_eq!(dx12.read_f32(Rid(2)).expect("read"), vec![2.0; 4]);
}

#[test]
fn dispatches_storage_buffer_writes() {
    let program =
        program(vec![Stmt::StoreBuffer { resource: 0, index: Expr::GlobalId, value: Expr::ConstF32(1.5) }, Stmt::Return(Expr::ConstF32(0.0))]);
    let graph = Graph {
        resources: ResourceGraph { nodes: vec![buffer(2, 4), buffer(3, 4)] },
        tasks: TaskGraph {
            nodes: vec![dispatch_task(
                12,
                &program,
                BTreeMap::from([("output".to_string(), ParamValue::I64(3)), ("buffer_0".to_string(), ParamValue::I64(2))]),
            )],
            edges: Vec::new(),
        },
    };

    let Some(mut dx12) = open()
    else {
        return;
    };
    dx12.prepare(&graph).expect("prepare");
    dx12.run_prepared(&graph).expect("dispatch");
    let actual = dx12.read_f32(Rid(2)).expect("read");
    assert_eq!(actual, vec![1.5; 4]);
}

#[test]
fn dispatches_uniform_only_program() {
    let program =
        program(vec![Stmt::Return(Expr::BinOp { op: BinOp::Add, lhs: Box::new(Expr::LoadUniform(0)), rhs: Box::new(Expr::ConstF32(0.5)) })]);
    let graph = Graph {
        resources: ResourceGraph { nodes: vec![buffer(2, 4)] },
        tasks: TaskGraph {
            nodes: vec![dispatch_task(
                11,
                &program,
                BTreeMap::from([("output".to_string(), ParamValue::I64(2)), ("uniform_0".to_string(), ParamValue::F64(1.5))]),
            )],
            edges: Vec::new(),
        },
    };

    let Some(mut dx12) = open()
    else {
        return;
    };
    dx12.prepare(&graph).expect("prepare");
    dx12.run_prepared(&graph).expect("dispatch");
    let actual = dx12.read_f32(Rid(2)).expect("read");
    assert_eq!(actual, vec![2.0; 4]);
}

/// A raw UAV write past the end of a buffer is the class of input that used to
/// crash `amdxc64.dll`; it must be rejected before dispatch.
#[test]
fn rejects_write_binding_smaller_than_the_dispatch() {
    let program =
        program(vec![Stmt::StoreBuffer { resource: 0, index: Expr::GlobalId, value: Expr::ConstF32(1.5) }, Stmt::Return(Expr::ConstF32(0.0))]);
    let graph = Graph {
        resources: ResourceGraph { nodes: vec![buffer(1, 2), buffer(2, 4)] },
        tasks: TaskGraph {
            nodes: vec![dispatch_task(
                21,
                &program,
                BTreeMap::from([("output".to_string(), ParamValue::I64(2)), ("buffer_0".to_string(), ParamValue::I64(1))]),
            )],
            edges: Vec::new(),
        },
    };

    let Some(mut dx12) = open()
    else {
        return;
    };
    dx12.prepare(&graph).expect("prepare");
    let error = dx12.run_prepared(&graph).expect_err("short write binding");
    assert!(error.to_string().contains("elements"), "{error}");
}

/// Raw SRVs carry no bounds, so an empty input must be rejected as well.
#[test]
fn rejects_empty_read_binding() {
    let program = program(vec![Stmt::Return(Expr::LoadBuffer { resource: 0, index: Box::new(Expr::GlobalId) })]);
    let graph = Graph {
        resources: ResourceGraph { nodes: vec![buffer(1, 0), buffer(2, 4)] },
        tasks: TaskGraph {
            nodes: vec![dispatch_task(
                22,
                &program,
                BTreeMap::from([("output".to_string(), ParamValue::I64(2)), ("buffer_0".to_string(), ParamValue::I64(1))]),
            )],
            edges: Vec::new(),
        },
    };

    let Some(mut dx12) = open()
    else {
        return;
    };
    if dx12.prepare(&graph).is_err() {
        return;
    }
    let error = dx12.run_prepared(&graph).expect_err("empty read binding");
    assert!(error.to_string().contains("empty") || error.to_string().contains("not allocated"), "{error}");
}
