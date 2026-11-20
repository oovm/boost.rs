//! Real-machine D3D12 end-to-end tests for the UCF integer/conversion instructions.
//!
//! Each program is dispatched on a usable D3D12 adapter (skipped with a message otherwise,
//! matching `program_dispatch.rs`) and the read-back words are compared bit-for-bit against
//! the CPU interpreter (`hardware_program::CpuInterpreter`) running the same program.

use std::collections::BTreeMap;

use hardware_backend_dx12::Dx12Backend;
use hardware_ir::{
    Access, Domain, Graph, ParamValue, Priority, ResourceGraph, ResourceId, ResourceKind, ResourceNode, ShaderId, TaskGraph, TaskId, TaskKind,
    TaskNode,
};
use hardware_program::{BinOp, CpuInterpreter, FunctionBuilder, Program, Rounding, ScalarTy, Ty, UnOp, Value, program_hash, validate_program};
use hardware_scheduler::Backend;
use hardware_types::{Dispatch, ResourceId as Rid};

fn buffer(id: u64, words: usize) -> ResourceNode {
    ResourceNode {
        id: ResourceId(id),
        kind: ResourceKind::Buffer,
        domain: Domain::Vram,
        access: Access::ReadWrite,
        byte_size: Some((words * 4) as u64),
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

fn u32_ty() -> Ty {
    Ty::Scalar(ScalarTy::U32)
}

/// 把 u32 结果按位回读为 f32 返回，GPU 侧 `read_f32` 即可拿到原始 32 位字。
fn finish(program: FunctionBuilder) -> Program {
    let function = program.finish().expect("function");
    let program = Program { version: 1, functions: vec![function], entry: 0 };
    validate_program(&program).expect("valid program");
    program
}

fn cpu_bits(program: &Program, buffers: Vec<(u32, Vec<Value>)>) -> Vec<u32> {
    let mut interpreter = CpuInterpreter::new();
    for (resource, values) in buffers {
        interpreter.with_buffer(resource, values);
    }
    interpreter
        .run_parallel(program, 4)
        .expect("cpu execute")
        .iter()
        .map(|result| match &result.value {
            Value::F32(value) => value.to_bits(),
            Value::U32(value) => *value,
            other => panic!("unexpected cpu result {other:?}"),
        })
        .collect()
}

fn gpu_bits(dx12: &mut Dx12Backend, graph: &Graph, seeds: Vec<(Rid, &[u32])>) -> Vec<u32> {
    dx12.prepare(graph).expect("prepare");
    for (id, words) in seeds {
        let floats: Vec<f32> = words.iter().map(|word| f32::from_bits(*word)).collect();
        dx12.write_f32(id, &floats).expect("seed");
    }
    dx12.run_prepared(graph).expect("dispatch");
    dx12.read_f32(Rid(2)).expect("read").iter().map(|value| value.to_bits()).collect()
}

fn open() -> Option<Dx12Backend> {
    match Dx12Backend::new() {
        Ok(backend) => Some(backend),
        Err(error) => {
            eprintln!("skip dx12 integer program test (no usable D3D12 device): {error}");
            None
        }
    }
}

#[test]
fn dispatches_utof_bitcast_and_integer_arithmetic() {
    // w = load_u32(gid); f = utof(w); b = bitcast(f); c = iadd(b, w); out = imul(c, 3)。
    let mut builder = FunctionBuilder::new("main");
    let gid = builder.emit_global_id().expect("gid");
    let word = builder.emit_load_buffer(0, gid, u32_ty()).expect("load");
    let as_float = builder.emit_unary(UnOp::U32ToF32, word, Ty::f32()).expect("utof");
    let bits = builder.emit_unary(UnOp::Bitcast, as_float, u32_ty()).expect("bitcast");
    let sum = builder.emit_binary(BinOp::Add, bits, word).expect("iadd");
    let three = builder.emit_const_u32(3).expect("const");
    let product = builder.emit_binary(BinOp::Mul, sum, three).expect("imul");
    let result = builder.emit_unary(UnOp::Bitcast, product, Ty::f32()).expect("to float bits");
    builder.return_value(result).expect("return");
    let program = finish(builder);

    let inputs: [u32; 4] = [0, 1, 0xdead_beef, u32::MAX];
    let graph = Graph {
        resources: ResourceGraph { nodes: vec![buffer(1, 4), buffer(2, 4)] },
        tasks: TaskGraph {
            nodes: vec![dispatch_task(
                40,
                &program,
                BTreeMap::from([("output".to_string(), ParamValue::I64(2)), ("buffer_0".to_string(), ParamValue::I64(1))]),
            )],
            edges: Vec::new(),
        },
    };
    let expected = cpu_bits(&program, vec![(0, inputs.iter().map(|word| Value::U32(*word)).collect())]);
    let Some(mut dx12) = open()
    else {
        return;
    };
    let actual = gpu_bits(&mut dx12, &graph, vec![(Rid(1), &inputs)]);
    assert_eq!(actual, expected);
}

#[test]
fn dispatches_ftou_trunc_and_round_with_saturation() {
    // x = load_f32(gid); t = f2i_trunc(x); r = f2i_round(x); out = bitcast(iadd(t, r))。
    // 覆盖 ftou 饱和/NaN→0 与 round 序列（round_ni/round_pi + movc）的平局远离零语义。
    let mut builder = FunctionBuilder::new("main");
    let gid = builder.emit_global_id().expect("gid");
    let value = builder.emit_load_buffer(0, gid, Ty::f32()).expect("load");
    let truncated = builder.emit_unary(UnOp::F32ToU32(Rounding::Trunc), value, u32_ty()).expect("trunc");
    let rounded = builder.emit_unary(UnOp::F32ToU32(Rounding::Round), value, u32_ty()).expect("round");
    let sum = builder.emit_binary(BinOp::Add, truncated, rounded).expect("iadd");
    let result = builder.emit_unary(UnOp::Bitcast, sum, Ty::f32()).expect("to float bits");
    builder.return_value(result).expect("return");
    let program = finish(builder);

    let floats: [f32; 4] = [2.7, f32::NAN, 126.5, 4294967040.5];
    let inputs: Vec<u32> = floats.iter().map(|value| value.to_bits()).collect();
    let graph = Graph {
        resources: ResourceGraph { nodes: vec![buffer(1, 4), buffer(2, 4)] },
        tasks: TaskGraph {
            nodes: vec![dispatch_task(
                41,
                &program,
                BTreeMap::from([("output".to_string(), ParamValue::I64(2)), ("buffer_0".to_string(), ParamValue::I64(1))]),
            )],
            edges: Vec::new(),
        },
    };
    let expected = cpu_bits(&program, vec![(0, floats.iter().map(|value| Value::F32(*value)).collect())]);
    // 显式钉死语义：trunc(2.7)=2、round(2.7)=3 → 5；NaN 两侧都饱和为 0；126.5 平局远离零得 127。
    assert_eq!(expected, vec![5, 0, 126 + 127, 4294967040u32.wrapping_add(4294967040)]);
    let Some(mut dx12) = open()
    else {
        return;
    };
    let actual = gpu_bits(&mut dx12, &graph, vec![(Rid(1), &inputs)]);
    assert_eq!(actual, expected);
}

#[test]
fn dispatches_rgba8_byte_unpack_with_and_or_shr() {
    // 线程 i 解包 RGBA8 字的第 i 字节：byte = (word >> (8*i)) & 0xff，再 or 上 0x100 标记。
    let mut builder = FunctionBuilder::new("main");
    let gid = builder.emit_global_id().expect("gid");
    let word = builder.emit_load_buffer(0, gid, u32_ty()).expect("load");
    let eight = builder.emit_const_u32(8).expect("const");
    let shift = builder.emit_binary(BinOp::Mul, gid, eight).expect("mul");
    let moved = builder.emit_binary(BinOp::Shr, word, shift).expect("shr");
    let mask = builder.emit_const_u32(0xff).expect("const");
    let byte = builder.emit_binary(BinOp::And, moved, mask).expect("and");
    let marker = builder.emit_const_u32(0x100).expect("const");
    let tagged = builder.emit_binary(BinOp::Or, byte, marker).expect("or");
    let result = builder.emit_unary(UnOp::Bitcast, tagged, Ty::f32()).expect("to float bits");
    builder.return_value(result).expect("return");
    let program = finish(builder);

    let inputs: [u32; 4] = [0xddcc_bb_aa, 0x1122_3344, 0, 0xffff_ffff];
    let graph = Graph {
        resources: ResourceGraph { nodes: vec![buffer(1, 4), buffer(2, 4)] },
        tasks: TaskGraph {
            nodes: vec![dispatch_task(
                42,
                &program,
                BTreeMap::from([("output".to_string(), ParamValue::I64(2)), ("buffer_0".to_string(), ParamValue::I64(1))]),
            )],
            edges: Vec::new(),
        },
    };
    let expected = cpu_bits(&program, vec![(0, inputs.iter().map(|word| Value::U32(*word)).collect())]);
    let expected_bytes: Vec<u32> = (0..4).map(|i| ((inputs[i] >> (8 * i)) & 0xff) | 0x100).collect();
    assert_eq!(expected, expected_bytes);
    let Some(mut dx12) = open()
    else {
        return;
    };
    let actual = gpu_bits(&mut dx12, &graph, vec![(Rid(1), &inputs)]);
    assert_eq!(actual, expected);
}

#[test]
fn dispatches_shift_saturation_at_thirty_two() {
    // out = or(shl(v, s), shr(v, s))，s = [16, 32, 33, 64]。
    // IR 层约定移位量 >= 32 结果为 0（DXBC 未定义，真机在此钉死行为）。
    let mut builder = FunctionBuilder::new("main");
    let gid = builder.emit_global_id().expect("gid");
    let value = builder.emit_load_buffer(0, gid, u32_ty()).expect("load");
    let amount = builder.emit_load_buffer(1, gid, u32_ty()).expect("load");
    let left = builder.emit_binary(BinOp::Shl, value, amount).expect("shl");
    let right = builder.emit_binary(BinOp::Shr, value, amount).expect("shr");
    let combined = builder.emit_binary(BinOp::Or, left, right).expect("or");
    let result = builder.emit_unary(UnOp::Bitcast, combined, Ty::f32()).expect("to float bits");
    builder.return_value(result).expect("return");
    let program = finish(builder);

    let values: [u32; 4] = [0xffff_ffff; 4];
    let amounts: [u32; 4] = [16, 32, 33, 64];
    let graph = Graph {
        resources: ResourceGraph { nodes: vec![buffer(1, 4), buffer(2, 4), buffer(3, 4)] },
        tasks: TaskGraph {
            nodes: vec![dispatch_task(
                43,
                &program,
                BTreeMap::from([
                    ("output".to_string(), ParamValue::I64(2)),
                    ("buffer_0".to_string(), ParamValue::I64(1)),
                    ("buffer_1".to_string(), ParamValue::I64(3)),
                ]),
            )],
            edges: Vec::new(),
        },
    };
    let expected = cpu_bits(
        &program,
        vec![(0, values.iter().map(|word| Value::U32(*word)).collect()), (1, amounts.iter().map(|word| Value::U32(*word)).collect())],
    );
    assert_eq!(expected, vec![0xffff_ffff, 0, 0, 0]);
    let Some(mut dx12) = open()
    else {
        return;
    };
    let actual = gpu_bits(&mut dx12, &graph, vec![(Rid(1), &values), (Rid(3), &amounts)]);
    assert_eq!(actual, expected);
}
