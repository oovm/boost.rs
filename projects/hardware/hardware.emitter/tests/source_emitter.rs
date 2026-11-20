//! Contract 44 T0/T1: UCF → WGSL / HLSL / GLSL text emitter fixtures.

use hardware_emitter::{
    capability_matrix, emit_shader_source, ABI_VERSION, BufferElement, ParamTy, ResourceAccess, ResourceDecl, ShaderInterface, ShaderSource,
    ShaderStage, SourceError, SourceTarget, UniformDecl, PARAM_RESOURCE,
};
use hardware_program::{
    BinOp, CpuInterpreter, Expr, Function, FunctionBuilder, Program, ProgramBuilder, ScalarTy, Stmt, StructuredFunctionBuilder, Ty,
    UnOp, Value,
};

fn u32_ty() -> Ty {
    Ty::Scalar(ScalarTy::U32)
}

fn f32_ty() -> Ty {
    Ty::f32()
}

fn bool_ty() -> Ty {
    Ty::Scalar(ScalarTy::Bool)
}

fn structured_entry(f: StructuredFunctionBuilder) -> Program {
    ProgramBuilder::new().structured_entry(f, u32_ty(), Expr::ConstU32(0)).unwrap().finish().unwrap()
}

fn program_of(func: Function) -> Program {
    Program { version: 1, functions: vec![func], entry: 0 }
}

/// Logical resource 7 read-only, 9 write-only, matching the contract example.
fn copy_program() -> Program {
    let mut f = StructuredFunctionBuilder::new("main");
    f.store_buffer(9, Expr::GlobalId, Expr::LoadBuffer { resource: 7, index: Box::new(Expr::GlobalId) });
    structured_entry(f)
}

fn copy_iface() -> ShaderInterface {
    ShaderInterface {
        stage: ShaderStage::Compute,
        entry: "main".into(),
        workgroup: [64, 1, 1],
        abi_version: ABI_VERSION,
        program_version: 1,
        params: vec![],
        resources: vec![
            ResourceDecl { id: 7, name: "src".into(), access: ResourceAccess::ReadOnly, element: BufferElement::U32 },
            ResourceDecl { id: 9, name: "dst".into(), access: ResourceAccess::WriteOnly, element: BufferElement::U32 },
        ],
    }
}

/// `dst[i] = uniform[3]`, then `if uniform[0] > 1.5 { dst[i] = 7u }`.
/// Slot 0 is f32 (word 1) and slot 3 is u32 (word 4), so the params block
/// spans two `u32x4` word groups.
fn param_program() -> Program {
    program_of(Function {
        name: "main".into(),
        params: vec![],
        locals: vec![],
        body: vec![
            Stmt::StoreBuffer { resource: 1, index: Expr::GlobalId, value: Expr::LoadUniform(3) },
            Stmt::If {
                cond: Expr::BinOp { op: BinOp::Gt, lhs: Box::new(Expr::LoadUniform(0)), rhs: Box::new(Expr::ConstF32(1.5)) },
                then_body: vec![Stmt::StoreBuffer { resource: 1, index: Expr::GlobalId, value: Expr::ConstU32(7) }],
                else_body: vec![],
            },
            Stmt::Return(Expr::ConstU32(0)),
        ],
        ret: u32_ty(),
    })
}

fn param_iface() -> ShaderInterface {
    ShaderInterface {
        stage: ShaderStage::Compute,
        entry: "main".into(),
        workgroup: [64, 1, 1],
        abi_version: ABI_VERSION,
        program_version: 1,
        params: vec![UniformDecl { slot: 0, ty: ParamTy::F32 }, UniformDecl { slot: 3, ty: ParamTy::U32 }],
        resources: vec![ResourceDecl { id: 1, name: "dst".into(), access: ResourceAccess::WriteOnly, element: BufferElement::U32 }],
    }
}

fn emit(program: &Program, iface: &ShaderInterface, target: SourceTarget) -> ShaderSource {
    emit_shader_source(program, iface, target).unwrap_or_else(|error| panic!("{error}"))
}

fn line_of(source: &str, needle: &str) -> usize {
    source
        .lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("line containing {needle:?} missing from:\n{source}"))
}

// ---------------------------------------------------------------------------
// T0: frozen ABI, golden text, capability matrix, manual byte layout.
// ---------------------------------------------------------------------------

#[test]
fn t0_copy_wgsl_matches_contract_example() {
    let source = emit(&copy_program(), &copy_iface(), SourceTarget::Wgsl);
    let expected = concat!(
        "struct Params { words: vec4<u32>, }\n",
        "@group(0) @binding(0) var<uniform> params: Params;\n",
        "@group(0) @binding(1) var<storage, read> src: array<u32>;\n",
        "@group(0) @binding(2) var<storage, read_write> dst: array<u32>;\n",
        "\n",
        "@compute @workgroup_size(64)\n",
        "fn main(@builtin(global_invocation_id) id: vec3<u32>) {\n",
        "    if (id.x >= params.words.x) { return; }\n",
        "    dst[id.x] = src[id.x];\n",
        "}\n",
    );
    assert_eq!(source.source, expected);
    assert_eq!(source.entry_point, "main");
    assert_eq!(source.profile, "wgsl");
    assert_eq!(source.workgroup_size, [64, 1, 1]);
    assert_eq!(source.param_bytes, 16);
}

#[test]
fn t0_copy_hlsl_golden() {
    let source = emit(&copy_program(), &copy_iface(), SourceTarget::Hlsl);
    let expected = concat!(
        "cbuffer Params : register(b0) { uint4 words; };\n",
        "StructuredBuffer<uint> src : register(t0);\n",
        "RWStructuredBuffer<uint> dst : register(u0);\n",
        "\n",
        "[numthreads(64, 1, 1)]\n",
        "void main(uint3 id : SV_DispatchThreadID) {\n",
        "    if (id.x >= words.x) { return; }\n",
        "    dst[id.x] = src[id.x];\n",
        "}\n",
    );
    assert_eq!(source.source, expected);
    assert_eq!(source.profile, "cs_5_0");
}

#[test]
fn t0_copy_glsl_golden() {
    let source = emit(&copy_program(), &copy_iface(), SourceTarget::Glsl);
    let expected = concat!(
        "#version 430 core\n",
        "\n",
        "layout(std140, binding=0) uniform Params { uvec4 words; };\n",
        "layout(std430, binding=1) readonly buffer Src { uint src[]; };\n",
        "layout(std430, binding=2) writeonly buffer Dst { uint dst[]; };\n",
        "\n",
        "layout(local_size_x = 64) in;\n",
        "\n",
        "void main() {\n",
        "    uint id = gl_GlobalInvocationID.x;\n",
        "    if (id >= words.x) { return; }\n",
        "    dst[id] = src[id];\n",
        "}\n",
    );
    assert_eq!(source.source, expected);
    assert_eq!(source.profile, "430 core");
    assert_eq!(source.entry_point, "main");
}

#[test]
fn t0_copy_manifest_is_deterministic() {
    let source = emit(&copy_program(), &copy_iface(), SourceTarget::Wgsl);
    assert_eq!(
        source.bindings,
        vec![
            hardware_emitter::SlotMap { resource: PARAM_RESOURCE, class: "uniform", index: 0 },
            hardware_emitter::SlotMap { resource: 7, class: "storage", index: 1 },
            hardware_emitter::SlotMap { resource: 9, class: "storage", index: 2 },
        ]
    );
    assert_eq!(
        source.min_binding_sizes,
        vec![
            hardware_emitter::MinSize { resource: 7, element_bytes: 4, min_elements_static: 0, count_dependent: true },
            hardware_emitter::MinSize { resource: 9, element_bytes: 4, min_elements_static: 0, count_dependent: true },
        ]
    );
    assert_eq!(source.device_capabilities, vec![
        "compute-stage",
        "storage-buffer-read",
        "storage-buffer-read-write",
        "uniform-buffer-vec4-groups",
        "workgroup-size-64"
    ]);
}

#[test]
fn t0_sparse_ids_declared_out_of_order_still_bind_ascending() {
    let mut iface = copy_iface();
    iface.resources.reverse();
    let source = emit(&copy_program(), &iface, SourceTarget::Wgsl);
    assert_eq!(source.bindings[1].resource, 7);
    assert_eq!(source.bindings[1].index, 1);
    assert_eq!(source.bindings[2].resource, 9);
    assert_eq!(source.bindings[2].index, 2);
    let hlsl = emit(&copy_program(), &iface, SourceTarget::Hlsl);
    assert_eq!(hlsl.bindings[1], hardware_emitter::SlotMap { resource: 7, class: "srv", index: 0 });
    assert_eq!(hlsl.bindings[2], hardware_emitter::SlotMap { resource: 9, class: "uav", index: 0 });
}

#[test]
fn t0_scalar_params_span_word_groups_with_manual_layout() {
    let source = emit(&param_program(), &param_iface(), SourceTarget::Wgsl);
    // Slot 3 lives in word 4: group 1, lane x. Slot 0 lives in word 1: group 0, lane y.
    assert!(source.source.contains("dst[id.x] = params.words[1].x;"), "{}", source.source);
    assert!(source.source.contains("bitcast<f32>(params.words[0].y)"), "{}", source.source);
    assert_eq!(source.param_bytes, 32);
    assert_eq!(source.field_offsets[0].name, "count");
    assert_eq!(source.field_offsets[0].byte_offset, 0);
    assert_eq!(source.field_offsets[1], hardware_emitter::FieldOffset { name: "uniform_0".into(), byte_offset: 4 });
    assert_eq!(source.field_offsets[2], hardware_emitter::FieldOffset { name: "uniform_3".into(), byte_offset: 16 });
    let guard = line_of(&source.source, "return; }");
    assert!(source.source.lines().nth(guard).unwrap().contains("params.words[0].x"), "guard must read word 0 as count");

    let hlsl = emit(&param_program(), &param_iface(), SourceTarget::Hlsl);
    assert!(hlsl.source.contains("dst[id.x] = words[1].x;"), "{}", hlsl.source);
    assert!(hlsl.source.contains("asfloat(words[0].y)"), "{}", hlsl.source);
    let glsl = emit(&param_program(), &param_iface(), SourceTarget::Glsl);
    assert!(glsl.source.contains("dst[id] = words[1].x;"), "{}", glsl.source);
    assert!(glsl.source.contains("uintBitsToFloat(words[0].y)"), "{}", glsl.source);
}

#[test]
fn t0_illegal_mixed_type_store_is_rejected() {
    // Storing the f32 parameter into the u32 buffer must fail as an ABI mismatch.
    let mut f = StructuredFunctionBuilder::new("main");
    f.store_buffer(1, Expr::GlobalId, Expr::LoadUniform(0));
    let program = structured_entry(f);
    let error = emit_shader_source(&program, &param_iface(), SourceTarget::Wgsl).unwrap_err();
    assert!(matches!(error, SourceError::AbiMismatch { .. }), "{error}");
    assert!(error.to_string().contains("mixed type"), "{error}");
}

#[test]
fn t0_profile_abi_and_stage_mismatches_are_located() {
    let program = copy_program();
    let mut iface = copy_iface();
    iface.abi_version = ABI_VERSION + 1;
    let error = emit_shader_source(&program, &iface, SourceTarget::Wgsl).unwrap_err();
    assert!(matches!(error, SourceError::AbiMismatch { .. }), "{error}");

    let mut iface = copy_iface();
    iface.workgroup = [128, 1, 1];
    let error = emit_shader_source(&program, &iface, SourceTarget::Wgsl).unwrap_err();
    assert!(matches!(error, SourceError::AbiMismatch { .. }), "{error}");

    let mut iface = copy_iface();
    iface.program_version = 99;
    let error = emit_shader_source(&program, &iface, SourceTarget::Hlsl).unwrap_err();
    assert!(matches!(error, SourceError::AbiMismatch { .. }), "{error}");

    let mut iface = copy_iface();
    iface.stage = ShaderStage::Vertex;
    let error = emit_shader_source(&program, &iface, SourceTarget::Glsl).unwrap_err();
    assert!(matches!(error, SourceError::CapabilityMissing { .. }), "{error}");
    assert!(error.to_string().contains("raster stage contract not ready"), "{error}");
}

#[test]
fn t0_capability_matrix_is_frozen_and_sorted() {
    let rows = capability_matrix();
    assert_eq!(rows.len(), 60);
    let mut sorted = rows.clone();
    sorted.sort_by_key(|row| (row.target, row.feature));
    assert_eq!(rows, sorted);
    let support = |target: &str, feature: &str| -> bool {
        rows.iter().find(|row| row.target == target && row.feature == feature).unwrap().supported
    };
    for target in ["wgsl", "hlsl", "glsl"] {
        assert!(support(target, "scalar-u32"), "{target} u32");
        assert!(support(target, "scalar-f32"), "{target} f32");
        assert!(support(target, "scalar-bool"), "{target} bool");
        assert!(!support(target, "u32-div"), "{target} u32-div");
        assert!(!support(target, "u32-mod"), "{target} u32-mod");
        assert!(!support(target, "function-call"), "{target} call");
        assert!(!support(target, "cfg-back-edge"), "{target} backedge");
        assert!(!support(target, "indirect-index-unbounded"), "{target} indirect");
        assert!(!support(target, "raster-stage"), "{target} raster");
        assert!(!support(target, "vec3"), "{target} vec3");
        assert!(!support(target, "f64"), "{target} f64");
        assert!(!support(target, "i32"), "{target} i32");
    }
    assert!(support("wgsl", "f32-mod") && support("hlsl", "f32-mod") && !support("glsl", "f32-mod"));
    assert!(!support("wgsl", "isfinite") && support("hlsl", "isfinite") && support("glsl", "isfinite"));
}

// ---------------------------------------------------------------------------
// T1: WGSL vertical slice against the CPU reference interpreter.
// ---------------------------------------------------------------------------

#[test]
fn t1_copy_matches_cpu_interpreter_at_count_boundaries() {
    let program = copy_program();
    for count in [0u32, 1, 63, 64, 65] {
        let src: Vec<Value> = (0..count).map(|i| Value::U32(i.wrapping_mul(2654435761).wrapping_add(7))).collect();
        let mut interp = CpuInterpreter::new();
        interp.with_buffer(7, src.clone());
        interp.with_buffer(9, vec![Value::U32(0xDEAD_BEEF); count as usize]);
        interp.run_parallel(&program, count).unwrap();
        let out = interp.buffer(9).unwrap().to_vec();
        assert_eq!(out, src, "count {count}");
    }
}

#[test]
fn t1_copy_leaves_unwritten_tail_untouched() {
    // Host sizes buffers for count=64 as 65 elements; the tail must survive.
    let program = copy_program();
    let src: Vec<Value> = (0..65).map(|i| Value::U32(i)).collect();
    let mut interp = CpuInterpreter::new();
    interp.with_buffer(7, src);
    interp.with_buffer(9, vec![Value::U32(0xDEAD_BEEF); 65]);
    interp.run_parallel(&program, 64).unwrap();
    let out = interp.buffer(9).unwrap();
    assert_eq!(&out[..64], &(0..64).map(Value::U32).collect::<Vec<_>>()[..]);
    assert_eq!(out[64], Value::U32(0xDEAD_BEEF));
}

#[test]
fn t1_uniform_semantics_match_cpu_interpreter() {
    let program = param_program();
    // f32 param 2.0 > 1.5 → the conditional store wins → 7 everywhere.
    let mut interp = CpuInterpreter::new();
    interp.with_uniform(0, Value::F32(2.0));
    interp.with_uniform(3, Value::U32(99));
    interp.with_buffer(1, vec![Value::U32(0); 3]);
    interp.run_parallel(&program, 3).unwrap();
    assert_eq!(interp.buffer(1).unwrap(), &[Value::U32(7), Value::U32(7), Value::U32(7)][..]);
    // f32 param 1.0 ≤ 1.5 → the u32 uniform survives.
    let mut interp = CpuInterpreter::new();
    interp.with_uniform(0, Value::F32(1.0));
    interp.with_uniform(3, Value::U32(99));
    interp.with_buffer(1, vec![Value::U32(0); 2]);
    interp.run_parallel(&program, 2).unwrap();
    assert_eq!(interp.buffer(1).unwrap(), &[Value::U32(99), Value::U32(99)][..]);
}

#[test]
fn t1_add_uniform_kernel_matches_cpu_interpreter() {
    let mut f = StructuredFunctionBuilder::new("main");
    let sum = f.let_value(u32_ty(), Expr::BinOp { op: BinOp::Add, lhs: Box::new(Expr::LoadBuffer { resource: 0, index: Box::new(Expr::GlobalId) }), rhs: Box::new(Expr::LoadUniform(2)) });
    f.store_buffer(1, Expr::GlobalId, Expr::LoadLocal(sum));
    let program = structured_entry(f);
    let iface = ShaderInterface {
        stage: ShaderStage::Compute,
        entry: "main".into(),
        workgroup: [64, 1, 1],
        abi_version: ABI_VERSION,
        program_version: 1,
        params: vec![UniformDecl { slot: 2, ty: ParamTy::U32 }],
        resources: vec![
            ResourceDecl { id: 0, name: "src".into(), access: ResourceAccess::ReadOnly, element: BufferElement::U32 },
            ResourceDecl { id: 1, name: "dst".into(), access: ResourceAccess::WriteOnly, element: BufferElement::U32 },
        ],
    };
    let source = emit(&program, &iface, SourceTarget::Wgsl);
    assert!(source.source.contains("params.words.w"), "{}", source.source);
    assert!(source.source.contains("v0 = (src[id.x] + params.words.w);"), "{}", source.source);
    assert!(source.source.contains("dst[id.x] = v0;"), "{}", source.source);

    let mut interp = CpuInterpreter::new();
    interp.with_uniform(2, Value::U32(7));
    interp.with_buffer(0, vec![Value::U32(1), Value::U32(2), Value::U32(3)]);
    interp.with_buffer(1, vec![Value::U32(0); 3]);
    interp.run_parallel(&program, 3).unwrap();
    assert_eq!(interp.buffer(1).unwrap(), &[Value::U32(8), Value::U32(9), Value::U32(10)][..]);
}

#[test]
fn t1_for_loop_index_is_static_bounded() {
    let mut f = StructuredFunctionBuilder::new("main");
    f.bounded_for(4, |f, i| {
        f.store_buffer(1, Expr::LoadLocal(i), Expr::LoadBuffer { resource: 0, index: Box::new(Expr::LoadLocal(i)) });
    });
    let program = structured_entry(f);
    let iface = ShaderInterface {
        stage: ShaderStage::Compute,
        entry: "main".into(),
        workgroup: [64, 1, 1],
        abi_version: ABI_VERSION,
        program_version: 1,
        params: vec![],
        resources: vec![
            ResourceDecl { id: 0, name: "src".into(), access: ResourceAccess::ReadOnly, element: BufferElement::U32 },
            ResourceDecl { id: 1, name: "dst".into(), access: ResourceAccess::WriteOnly, element: BufferElement::U32 },
        ],
    };
    let source = emit(&program, &iface, SourceTarget::Wgsl);
    assert!(source.source.contains("for (var ucf_i0: u32 = 0u; ucf_i0 < 4u; ucf_i0 = ucf_i0 + 1u) {"), "{}", source.source);
    for size in &source.min_binding_sizes {
        assert_eq!(size.min_elements_static, 4, "{size:?}");
        assert!(!size.count_dependent, "{size:?}");
    }

    let src: Vec<Value> = (0..4).map(|i| Value::U32(i * 11)).collect();
    let mut interp = CpuInterpreter::new();
    interp.with_buffer(0, src.clone());
    interp.with_buffer(1, vec![Value::U32(0); 4]);
    interp.run_parallel(&program, 1).unwrap();
    assert_eq!(interp.buffer(1).unwrap(), src.as_slice());
}

#[test]
fn t1_while_loop_semantics_match_cpu_interpreter() {
    let mut f = StructuredFunctionBuilder::new("main");
    let acc = f.let_value(u32_ty(), Expr::ConstU32(0));
    f.bounded_while(Expr::BinOp { op: BinOp::Lt, lhs: Box::new(Expr::LoadLocal(acc)), rhs: Box::new(Expr::ConstU32(5)) }, 10, |f| {
        f.store_local(acc, Expr::BinOp { op: BinOp::Add, lhs: Box::new(Expr::LoadLocal(acc)), rhs: Box::new(Expr::ConstU32(1)) });
    });
    f.store_buffer(0, Expr::GlobalId, Expr::LoadLocal(acc));
    let program = structured_entry(f);
    let iface = ShaderInterface {
        stage: ShaderStage::Compute,
        entry: "main".into(),
        workgroup: [64, 1, 1],
        abi_version: ABI_VERSION,
        program_version: 1,
        params: vec![],
        resources: vec![ResourceDecl { id: 0, name: "dst".into(), access: ResourceAccess::WriteOnly, element: BufferElement::U32 }],
    };
    let source = emit(&program, &iface, SourceTarget::Wgsl);
    assert!(source.source.contains("while (ucf_g0 < 10u && (v0 < 5u))"), "{}", source.source);
    assert!(source.source.contains("ucf_g0 = ucf_g0 + 1u;"), "{}", source.source);

    let mut interp = CpuInterpreter::new();
    interp.with_buffer(0, vec![Value::U32(0); 2]);
    interp.run_parallel(&program, 2).unwrap();
    assert_eq!(interp.buffer(0).unwrap(), &[Value::U32(5), Value::U32(5)][..]);
}

#[test]
fn t1_ast_select_is_lazy_if_not_eager_builtin() {
    let mut f = StructuredFunctionBuilder::new("main");
    let cond = Expr::BinOp { op: BinOp::Lt, lhs: Box::new(Expr::GlobalId), rhs: Box::new(Expr::ConstU32(1)) };
    let selected = Expr::Select {
        cond: Box::new(cond),
        then_: Box::new(Expr::LoadBuffer { resource: 0, index: Box::new(Expr::GlobalId) }),
        else_: Box::new(Expr::LoadBuffer { resource: 1, index: Box::new(Expr::GlobalId) }),
    };
    let v = f.let_value(u32_ty(), selected);
    f.store_buffer(2, Expr::GlobalId, Expr::LoadLocal(v));
    let program = structured_entry(f);
    let iface = ShaderInterface {
        stage: ShaderStage::Compute,
        entry: "main".into(),
        workgroup: [64, 1, 1],
        abi_version: ABI_VERSION,
        program_version: 1,
        params: vec![],
        resources: vec![
            ResourceDecl { id: 0, name: "a".into(), access: ResourceAccess::ReadOnly, element: BufferElement::U32 },
            ResourceDecl { id: 1, name: "b".into(), access: ResourceAccess::ReadOnly, element: BufferElement::U32 },
            ResourceDecl { id: 2, name: "dst".into(), access: ResourceAccess::WriteOnly, element: BufferElement::U32 },
        ],
    };
    let source = emit(&program, &iface, SourceTarget::Wgsl);
    assert!(!source.source.contains("select("), "{}", source.source);
    assert!(source.source.contains("if ((id.x < 1u)) {"), "{}", source.source);
    assert!(source.source.contains("t0 = a[id.x];"), "{}", source.source);
    assert!(source.source.contains("t0 = b[id.x];"), "{}", source.source);

    let mut interp = CpuInterpreter::new();
    interp.with_buffer(0, vec![Value::U32(10), Value::U32(11)]);
    interp.with_buffer(1, vec![Value::U32(20), Value::U32(21)]);
    interp.with_buffer(2, vec![Value::U32(0); 2]);
    interp.run_parallel(&program, 2).unwrap();
    assert_eq!(interp.buffer(2).unwrap(), &[Value::U32(10), Value::U32(21)][..]);
}

/// Single-block SSA copy kernel.
fn cfg_copy_program() -> Program {
    let mut f = FunctionBuilder::new("main");
    let gid = f.emit_global_id().unwrap();
    let src = f.emit_load_buffer(7, gid, u32_ty()).unwrap();
    f.emit_store_buffer(9, gid, src).unwrap();
    f.return_value(src).unwrap();
    ProgramBuilder::new().entry(f).unwrap().finish().unwrap()
}

#[test]
fn t1_cfg_single_block_matches_cpu_interpreter() {
    let program = cfg_copy_program();
    let source = emit(&program, &copy_iface(), SourceTarget::Wgsl);
    assert!(source.source.contains("var s0: u32 = id.x;"), "{}", source.source);
    assert!(source.source.contains("var s1: u32 = src[s0];"), "{}", source.source);
    assert!(source.source.contains("dst[s0] = s1;"), "{}", source.source);

    let src: Vec<Value> = (0..3).map(|i| Value::U32(i * i)).collect();
    let mut interp = CpuInterpreter::new();
    interp.with_buffer(7, src.clone());
    interp.with_buffer(9, vec![Value::U32(0); 3]);
    interp.run_parallel(&program, 3).unwrap();
    assert_eq!(interp.buffer(9).unwrap(), src.as_slice());
}

/// `if (id < 1) { dst[id] = 42; return; } else { dst[id] = 1; return; }` as SSA.
fn cfg_branch_program() -> Program {
    let mut f = FunctionBuilder::new("main");
    let gid = f.emit_global_id().unwrap();
    let one = f.emit_const_u32(1).unwrap();
    let cond = f.emit_binary(BinOp::Lt, gid, one).unwrap();
    let then_block = f.create_block();
    let else_block = f.create_block();
    f.conditional_branch(cond, then_block, else_block).unwrap();
    f.switch_to(then_block).unwrap();
    let forty_two = f.emit_const_u32(42).unwrap();
    f.emit_store_buffer(9, gid, forty_two).unwrap();
    f.return_value(forty_two).unwrap();
    f.switch_to(else_block).unwrap();
    f.emit_store_buffer(9, gid, one).unwrap();
    f.return_value(one).unwrap();
    ProgramBuilder::new().entry(f).unwrap().finish().unwrap()
}

#[test]
fn t1_cfg_branch_recovers_as_if_else_and_matches_cpu() {
    let program = cfg_branch_program();
    let source = emit(&program, &copy_iface(), SourceTarget::Wgsl);
    assert!(source.source.contains("if (s2) {"), "{}", source.source);
    assert!(source.source.contains("} else {"), "{}", source.source);
    assert!(source.source.contains("dst[s0] = s3;"), "{}", source.source);
    assert!(source.source.contains("dst[s0] = s1;"), "{}", source.source);

    let mut interp = CpuInterpreter::new();
    interp.with_buffer(9, vec![Value::U32(0); 2]);
    interp.with_buffer(7, vec![Value::U32(0); 2]);
    interp.run_parallel(&program, 2).unwrap();
    assert_eq!(interp.buffer(9).unwrap(), &[Value::U32(42), Value::U32(1)][..]);
}

/// Both branches converge into one merge block with a phi parameter.
fn cfg_merge_program() -> Program {
    let mut f = FunctionBuilder::new("main");
    let gid = f.emit_global_id().unwrap();
    let one = f.emit_const_u32(1).unwrap();
    let cond = f.emit_binary(BinOp::Lt, gid, one).unwrap();
    let then_block = f.create_block();
    let else_block = f.create_block();
    let merge = f.create_block();
    let phi = f.block_parameter(merge, u32_ty()).unwrap();
    f.conditional_branch(cond, then_block, else_block).unwrap();
    f.switch_to(then_block).unwrap();
    let five = f.emit_const_u32(5).unwrap();
    f.branch_with_args(merge, vec![five]).unwrap();
    f.switch_to(else_block).unwrap();
    let six = f.emit_const_u32(6).unwrap();
    f.branch_with_args(merge, vec![six]).unwrap();
    f.switch_to(merge).unwrap();
    f.emit_store_buffer(9, gid, phi).unwrap();
    f.return_value(phi).unwrap();
    ProgramBuilder::new().entry(f).unwrap().finish().unwrap()
}

#[test]
fn t1_cfg_merge_recovers_with_parallel_parameter_assignment() {
    let program = cfg_merge_program();
    let source = emit(&program, &copy_iface(), SourceTarget::Wgsl);
    assert!(source.source.contains("var s3: u32;"), "{}", source.source);
    assert!(source.source.contains("} else {"), "{}", source.source);
    assert!(source.source.contains("dst[s0] = s3;"), "{}", source.source);

    let mut interp = CpuInterpreter::new();
    interp.with_buffer(9, vec![Value::U32(0); 2]);
    interp.with_buffer(7, vec![Value::U32(0); 2]);
    interp.run_parallel(&program, 2).unwrap();
    assert_eq!(interp.buffer(9).unwrap(), &[Value::U32(5), Value::U32(6)][..]);
}

#[test]
fn t1_cfg_unrecoverable_shapes_are_rejected() {
    // Conditional branch whose two arms converge into different blocks.
    let mut f = FunctionBuilder::new("main");
    let gid = f.emit_global_id().unwrap();
    let one = f.emit_const_u32(1).unwrap();
    let cond = f.emit_binary(BinOp::Lt, gid, one).unwrap();
    let then_block = f.create_block();
    let else_block = f.create_block();
    let merge_a = f.create_block();
    let merge_b = f.create_block();
    f.conditional_branch(cond, then_block, else_block).unwrap();
    f.switch_to(then_block).unwrap();
    f.branch(merge_a).unwrap();
    f.switch_to(else_block).unwrap();
    f.branch(merge_b).unwrap();
    f.switch_to(merge_a).unwrap();
    f.emit_store_buffer(9, gid, one).unwrap();
    f.return_value(one).unwrap();
    f.switch_to(merge_b).unwrap();
    f.emit_store_buffer(9, gid, one).unwrap();
    f.return_value(one).unwrap();
    let program = ProgramBuilder::new().entry(f).unwrap().finish().unwrap();
    let error = emit_shader_source(&program, &copy_iface(), SourceTarget::Wgsl).unwrap_err();
    assert!(matches!(error, SourceError::UnrecoverableControlFlow { .. }), "{error}");

    // Back edge: then-arm jumps to a block that loops into the entry.
    let mut f = FunctionBuilder::new("main");
    let gid = f.emit_global_id().unwrap();
    let one = f.emit_const_u32(1).unwrap();
    let cond = f.emit_binary(BinOp::Lt, gid, one).unwrap();
    let body = f.create_block();
    let exit = f.create_block();
    f.conditional_branch(cond, body, exit).unwrap();
    f.switch_to(body).unwrap();
    f.branch(f.entry_block()).unwrap();
    f.switch_to(exit).unwrap();
    f.emit_store_buffer(9, gid, one).unwrap();
    f.return_value(one).unwrap();
    let program = ProgramBuilder::new().entry(f).unwrap().finish().unwrap();
    let error = emit_shader_source(&program, &copy_iface(), SourceTarget::Wgsl).unwrap_err();
    assert!(matches!(error, SourceError::UnrecoverableControlFlow { .. }), "{error}");
}

#[test]
fn t1_unsupported_ops_are_rejected_per_target() {
    let div = |lhs: Expr, rhs: Expr| Expr::BinOp { op: BinOp::Div, lhs: Box::new(lhs), rhs: Box::new(rhs) };
    let mut f = StructuredFunctionBuilder::new("main");
    f.store_buffer(9, div(Expr::LoadBuffer { resource: 7, index: Box::new(Expr::GlobalId) }, Expr::ConstU32(2)), Expr::ConstU32(0));
    let program = structured_entry(f);
    let error = emit_shader_source(&program, &copy_iface(), SourceTarget::Wgsl).unwrap_err();
    assert!(matches!(error, SourceError::CapabilityMissing { .. }), "{error}");

    // f32 %: fine in WGSL/HLSL, rejected in GLSL (floor-mod semantics).
    let mut f = StructuredFunctionBuilder::new("main");
    let r = f.let_value(f32_ty(), Expr::BinOp { op: BinOp::Mod, lhs: Box::new(Expr::ConstF32(1.0)), rhs: Box::new(Expr::ConstF32(2.0)) });
    f.store_buffer(9, Expr::GlobalId, Expr::ConstU32(0));
    let _ = r;
    let program = structured_entry(f);
    let wgsl = emit(&program, &copy_iface(), SourceTarget::Wgsl);
    assert!(wgsl.source.contains("mod("), "{}", wgsl.source);
    let error = emit_shader_source(&program, &copy_iface(), SourceTarget::Glsl).unwrap_err();
    assert!(matches!(error, SourceError::CapabilityMissing { .. }), "{error}");

    // isfinite: rejected in WGSL, emitted in HLSL.
    let program = program_of(Function {
        name: "main".into(),
        params: vec![],
        locals: vec![(0, bool_ty())],
        body: vec![
            Stmt::Let { id: 0, ty: bool_ty(), init: Expr::UnOp { op: UnOp::IsFinite, arg: Box::new(Expr::ConstF32(1.0)) } },
            Stmt::If { cond: Expr::LoadLocal(0), then_body: vec![Stmt::StoreBuffer { resource: 9, index: Expr::GlobalId, value: Expr::ConstU32(1) }], else_body: vec![] },
            Stmt::Return(Expr::ConstU32(0)),
        ],
        ret: u32_ty(),
    });
    let error = emit_shader_source(&program, &copy_iface(), SourceTarget::Wgsl).unwrap_err();
    assert!(matches!(error, SourceError::CapabilityMissing { .. }), "{error}");
    let hlsl = emit(&program, &copy_iface(), SourceTarget::Hlsl);
    assert!(hlsl.source.contains("isfinite("), "{}", hlsl.source);
}

#[test]
fn t1_unbounded_indirect_index_is_rejected() {
    let mut f = StructuredFunctionBuilder::new("main");
    let idx = f.let_value(u32_ty(), Expr::BinOp { op: BinOp::Add, lhs: Box::new(Expr::GlobalId), rhs: Box::new(Expr::ConstU32(1)) });
    f.store_buffer(9, Expr::LoadLocal(idx), Expr::ConstU32(0));
    let program = structured_entry(f);
    let error = emit_shader_source(&program, &copy_iface(), SourceTarget::Wgsl).unwrap_err();
    assert!(matches!(error, SourceError::CapabilityMissing { .. }), "{error}");
    assert!(error.to_string().contains("indirect"), "{error}");
}

#[test]
fn t1_access_violations_and_missing_declarations_are_abi_errors() {
    let program = copy_program();

    // Read of a write-only resource.
    let mut iface = copy_iface();
    iface.resources[1].access = ResourceAccess::ReadOnly;
    let error = emit_shader_source(&program, &iface, SourceTarget::Wgsl).unwrap_err();
    assert!(matches!(error, SourceError::AbiMismatch { .. }), "{error}");

    // Write to a read-only resource.
    let mut iface = copy_iface();
    iface.resources[0].access = ResourceAccess::WriteOnly;
    let error = emit_shader_source(&program, &iface, SourceTarget::Wgsl).unwrap_err();
    assert!(matches!(error, SourceError::AbiMismatch { .. }), "{error}");

    // Undeclared resource.
    let mut iface = copy_iface();
    iface.resources.remove(0);
    let error = emit_shader_source(&program, &iface, SourceTarget::Wgsl).unwrap_err();
    assert!(matches!(error, SourceError::AbiMismatch { .. }), "{error}");

    // Undeclared uniform slot.
    let error = emit_shader_source(&param_program(), &copy_iface(), SourceTarget::Wgsl).unwrap_err();
    assert!(matches!(error, SourceError::AbiMismatch { .. }), "{error}");
}

#[test]
fn t1_read_before_definite_assignment_is_invalid_ir_with_location() {
    let func = Function {
        name: "main".into(),
        params: vec![],
        locals: vec![(0, u32_ty()), (1, u32_ty())],
        body: vec![
            Stmt::StoreLocal { id: 0, value: Expr::ConstU32(5) },
            Stmt::Let { id: 1, ty: u32_ty(), init: Expr::LoadLocal(1) },
            Stmt::Return(Expr::ConstU32(0)),
        ],
        ret: u32_ty(),
    };
    let program = Program { version: 1, functions: vec![func], entry: 0 };
    let error = emit_shader_source(&program, &copy_iface(), SourceTarget::Wgsl).unwrap_err();
    assert!(matches!(error, SourceError::InvalidIr { .. }), "{error}");
    let text = error.to_string();
    assert!(text.contains("wgsl"), "{text}");
    assert!(text.contains("main"), "{text}");
    assert!(text.contains("statement 1"), "{text}");
    let location = error.location().unwrap();
    assert_eq!(location.statement, Some(1));
    assert_eq!(location.block, None);
}

#[test]
fn t1_source_map_lines_resolve_to_the_emitted_text() {
    let source = emit(&copy_program(), &copy_iface(), SourceTarget::Wgsl);
    assert!(!source.source_map.is_empty());
    let lines: Vec<&str> = source.source.lines().collect();
    for location in &source.source_map {
        assert_eq!(location.function, "main");
        let line = lines[(location.line - 1) as usize];
        assert!(!line.trim().is_empty(), "mapped line {} is blank", location.line);
    }
    let store = source.source_map.iter().find(|l| l.statement == Some(0)).unwrap();
    assert!(lines[(store.line - 1) as usize].contains("dst[id.x] = src[id.x];"), "{}", lines[(store.line - 1) as usize]);
}

#[test]
fn t1_hash_is_stable_and_identity_sensitive() {
    let program = copy_program();
    let a = emit(&program, &copy_iface(), SourceTarget::Wgsl);
    let b = emit(&program, &copy_iface(), SourceTarget::Wgsl);
    assert_eq!(a.hash, b.hash);
    assert_eq!(a.hash.len(), 16);
    assert!(a.hash.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    let hlsl = emit(&program, &copy_iface(), SourceTarget::Hlsl);
    assert_ne!(a.hash, hlsl.hash);
    let other = cfg_copy_program();
    let c = emit(&other, &copy_iface(), SourceTarget::Wgsl);
    assert_ne!(a.hash, c.hash);
}

// ---------------------------------------------------------------------------
// T1: browser self-test artifact (tests/webgpu/index.html).
// ---------------------------------------------------------------------------

/// Builds the self-contained WebGPU page: the WGSL blocks are emitter output
/// byte for byte, the expectations are hand-computed in plain JavaScript.
fn webgpu_self_test_html() -> String {
    let copy = emit(&copy_program(), &copy_iface(), SourceTarget::Wgsl);
    let param = emit(&param_program(), &param_iface(), SourceTarget::Wgsl);
    assert_eq!(copy.entry_point, "main");
    assert_eq!(param.entry_point, "main");
    r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>UCF text emitter — WebGPU compute self-test</title>
<style>
  body { font: 14px/1.5 ui-monospace, Consolas, monospace; margin: 24px; background: #101418; color: #d7e0e8; }
  h1 { font-size: 18px; margin-bottom: 4px; }
  #summary { font-weight: bold; }
  #summary.pass { color: #6fe08a; }
  #summary.fail { color: #ff7b72; }
  #log { white-space: pre-wrap; background: #0a0e12; border: 1px solid #2a3540; padding: 12px; }
</style>
</head>
<body>
<h1>UCF text emitter — WebGPU compute self-test</h1>
<p>Generated by <code>tests/source_emitter.rs</code>. The WGSL below is emitter output, byte for byte; the expectations are computed in plain JavaScript. No network access is used.</p>
<p id="summary">running…</p>
<pre id="log"></pre>

<script id="copy-wgsl" type="text/wgsl">/*COPY_WGSL*/</script>
<script id="param-wgsl" type="text/wgsl">/*PARAM_WGSL*/</script>

<script>
'use strict';
const WORKGROUP = /*WORKGROUP*/;
const COPY_PARAM_BYTES = /*COPY_PARAMS*/;
const PARAM_PARAM_BYTES = /*PARAM_PARAMS*/;
const SENTINEL = 0xDEADBEEF;
const CAP = 66;
const COUNTS = [0, 1, 63, 64, 65];

const logEl = document.getElementById('log');
const summaryEl = document.getElementById('summary');
let checks = 0;
let failures = 0;
function log(line) { logEl.textContent += line + '\n'; }
function check(ok, name) {
  checks++;
  if (!ok) failures++;
  log((ok ? 'PASS' : 'FAIL') + '  ' + name);
}

// Same pattern as the Rust CPU-parity fixtures: (i * 2654435761 + 7) mod 2^32.
function srcPattern(i) { return (Math.imul(i, 2654435761) + 7) >>> 0; }
function f32Bits(x) {
  const view = new DataView(new ArrayBuffer(4));
  view.setFloat32(0, x, true);
  return view.getUint32(0, true);
}

const STORAGE = GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_DST | GPUBufferUsage.COPY_SRC | GPUBufferUsage.MAP_READ;
const UNIFORM = GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST;

function write(device, values, usage) {
  const buffer = device.createBuffer({ size: Math.max(values.byteLength, 4), usage });
  device.queue.writeBuffer(buffer, 0, values);
  return buffer;
}

async function readBack(device, buffer) {
  await buffer.mapAsync(GPUMapMode.READ);
  const out = new Uint32Array(buffer.getMappedRange().slice(0));
  buffer.unmap();
  return out;
}

async function compile(device, id) {
  const module = device.createShaderModule({ code: document.getElementById(id).textContent });
  return device.createComputePipelineAsync({ layout: 'auto', compute: { module, entryPoint: 'main' } });
}

async function dispatch(device, pipeline, entries, count) {
  const bindGroup = device.createBindGroup({ layout: pipeline.getBindGroupLayout(0), entries });
  const encoder = device.createCommandEncoder();
  const pass = encoder.beginComputePass();
  pass.setPipeline(pipeline);
  pass.setBindGroup(0, bindGroup);
  pass.dispatchWorkgroups(Math.ceil(count / WORKGROUP));
  pass.end();
  device.queue.submit([encoder.finish()]);
}

async function runCopyCase(device, pipeline, count) {
  const src = write(device, Uint32Array.from({ length: CAP }, (_, i) => srcPattern(i)), STORAGE);
  const dst = write(device, new Uint32Array(CAP).fill(SENTINEL), STORAGE);
  const params = write(device, new Uint32Array([count, 0, 0, 0]), UNIFORM);
  await dispatch(device, pipeline, [
    { binding: 0, resource: { buffer: params } },
    { binding: 1, resource: { buffer: src } },
    { binding: 2, resource: { buffer: dst } },
  ], count);
  const out = await readBack(device, dst);
  let ok = true;
  for (let i = 0; i < CAP; i++) {
    const want = i < count ? srcPattern(i) : SENTINEL;
    if (out[i] !== want) {
      ok = false;
      log('  copy mismatch at ' + i + ': got 0x' + out[i].toString(16) + ' want 0x' + want.toString(16));
    }
  }
  check(ok, 'copy kernel, count=' + count + ' (tail untouched)');
  src.destroy(); dst.destroy(); params.destroy();
}

async function runParamCase(device, pipeline, count, u0, u3) {
  const dst = write(device, new Uint32Array(CAP).fill(SENTINEL), STORAGE);
  const words = new Uint32Array(PARAM_PARAM_BYTES / 4);
  words[0] = count;
  words[1] = f32Bits(u0);
  words[4] = u3;
  const params = write(device, words, UNIFORM);
  await dispatch(device, pipeline, [
    { binding: 0, resource: { buffer: params } },
    { binding: 1, resource: { buffer: dst } },
  ], count);
  const out = await readBack(device, dst);
  const eager = Math.fround(u0) > 1.5 ? 7 : u3;
  let ok = true;
  for (let i = 0; i < CAP; i++) {
    const want = i < count ? eager : SENTINEL;
    if (out[i] !== want) {
      ok = false;
      log('  param mismatch at ' + i + ': got 0x' + out[i].toString(16) + ' want 0x' + want.toString(16));
    }
  }
  check(ok, 'param kernel, count=' + count + ', u0=' + u0 + ', u3=' + u3);
  dst.destroy(); params.destroy();
}

async function main() {
  try {
    if (!navigator.gpu) throw new Error('WebGPU unavailable in this browser');
    const adapter = await navigator.gpu.requestAdapter();
    if (!adapter) throw new Error('no suitable GPU adapter');
    const device = await adapter.requestDevice();
    if (COPY_PARAM_BYTES !== 16) throw new Error('unexpected copy params size');
    const copy = await compile(device, 'copy-wgsl');
    for (const count of COUNTS) await runCopyCase(device, copy, count);
    const param = await compile(device, 'param-wgsl');
    for (const count of COUNTS) {
      await runParamCase(device, param, count, 2.0, 42);
      await runParamCase(device, param, count, 1.0, 42);
    }
  } catch (error) {
    check(false, 'fatal: ' + error);
  }
  if (failures === 0) {
    summaryEl.textContent = 'ALL PASS (' + checks + ' checks)';
    summaryEl.className = 'pass';
  } else {
    summaryEl.textContent = failures + ' of ' + checks + ' checks FAILED';
    summaryEl.className = 'fail';
  }
}
main();
</script>
</body>
</html>
"##
        .replace("/*COPY_WGSL*/", &copy.source)
        .replace("/*PARAM_WGSL*/", &param.source)
        .replace("/*WORKGROUP*/", &copy.workgroup_size[0].to_string())
        .replace("/*COPY_PARAMS*/", &copy.param_bytes.to_string())
        .replace("/*PARAM_PARAMS*/", &param.param_bytes.to_string())
}

#[test]
fn t1_webgpu_self_test_page_is_generated_from_emitter_output() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("webgpu");
    let path = dir.join("index.html");
    let expected = webgpu_self_test_html();
    if std::fs::read_to_string(&path).ok().as_deref() != Some(expected.as_str()) {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, &expected).unwrap();
    }
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_eq!(on_disk, expected, "index.html must stay byte-identical to emitter output");
    assert!(on_disk.contains("dst[id.x] = src[id.x];"));
    assert!(on_disk.contains("dst[id.x] = params.words[1].x;"));
    assert!(on_disk.contains("bitcast<f32>(params.words[0].y)"));
    assert!(!on_disk.contains("http://") && !on_disk.contains("https://"), "self-test page must not reach the network");
}
