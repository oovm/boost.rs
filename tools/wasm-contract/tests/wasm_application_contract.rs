//! 通过 `wasmtime` 加载 `hardware_wasm.wasm` 并验收 C ABI 应用契约。
//!
//! 本文件用 `harness = false`：产物缺失时必须把跳过原因直接打到终端，
//! 不能像默认 libtest 那样把输出吞掉后仍显示为「通过」。

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

use wasmtime::{Engine, Instance, Linker, Module, Store, TypedFunc};

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("repository root")
}

/// 定位已构建的 Wasm 产物；缺失时打印跳过原因并返回 `None`。
fn wasm_artifact() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("HARDWARE_WASM_ARTIFACT") {
        let path = PathBuf::from(path);
        assert!(path.is_file(), "`HARDWARE_WASM_ARTIFACT` points to a missing file: {}", path.display());
        return Some(path);
    }
    let default = repository_root().join("target/wasm32-unknown-unknown/release/hardware_wasm.wasm");
    if default.is_file() {
        return Some(default);
    }
    println!(
        "SKIP: `{}` 不存在，未执行任何 C ABI 契约断言。\n\
         先执行 `rustup target add wasm32-unknown-unknown` 与 \
         `cargo build -p hardware-wasm --target wasm32-unknown-unknown --release`，\
         或用 `HARDWARE_WASM_ARTIFACT` 指向已有产物后重跑 \
         `cargo test --manifest-path tools/wasm-contract/Cargo.toml`。",
        default.display()
    );
    None
}

fn load_instance(path: &Path) -> (Store<()>, Instance) {
    let engine = Engine::default();
    let module = Module::from_file(&engine, path).expect("load wasm module");
    let mut store = Store::new(&engine, ());
    let mut linker = Linker::new(&engine);
    linker.func_wrap("ucf_host", "ucf_host_bind_external_buffer", |_: i32, _: i32| -> i32 { 0 }).expect("define ucf_host_bind_external_buffer");
    linker
        .func_wrap("ucf_host", "ucf_host_l2_color_fill_webgpu", |_: i32, _: i32, _: i32, _: i32, _: i32, _: i32| -> i32 { 0 })
        .expect("define ucf_host_l2_color_fill_webgpu");
    linker
        .func_wrap("ucf_host", "ucf_host_l2_raster_clear_webgpu", |_: i32, _: i32, _: i32, _: i32, _: i32, _: i32| -> i32 { 0 })
        .expect("define ucf_host_l2_raster_clear_webgpu");
    linker.func_wrap("ucf_host", "ucf_host_l2_raster_tri_webgpu", |_: i32, _: i32| -> i32 { 0 }).expect("define ucf_host_l2_raster_tri_webgpu");
    let instance = linker.instantiate(&mut store, &module).expect("instantiate hardware_wasm");
    (store, instance)
}

fn exports_application_contract_matmul(path: &Path) {
    let (mut store, instance) = load_instance(path);

    let has_cpu: TypedFunc<(), i32> = instance.get_typed_func(&mut store, "ucf_has_cpu_backend").expect("ucf_has_cpu_backend");
    assert_eq!(has_cpu.call(&mut store, ()).expect("call"), 1);

    let triple = instance.get_func(&mut store, "ucf_target_triple").expect("ucf_target_triple");
    let mut results = [wasmtime::Val::I32(0)];
    triple.call(&mut store, &[], &mut results).expect("call triple");
    let ptr = match results[0] {
        wasmtime::Val::I32(p) => p as u32,
        _ => panic!("unexpected triple return"),
    };
    assert!(ptr > 0);

    let matmul: TypedFunc<(i32,), i32> =
        instance.get_typed_func(&mut store, "ucf_application_contract_matmul").expect("ucf_application_contract_matmul");

    let memory = instance.get_memory(&mut store, "memory").expect("export memory");
    let out_ptr: i32 = 64;
    for i in 0..4 {
        memory.write(&mut store, (out_ptr as usize + i * 4), &0f32.to_le_bytes()).expect("init out slot");
    }

    let status = matmul.call(&mut store, (out_ptr,)).expect("matmul contract");
    assert_eq!(status, 0, "contract failed with code {status}");

    let mut buf = [0u8; 4];
    let mut out = [0f32; 4];
    for (i, slot) in out.iter_mut().enumerate() {
        memory.read(&store, out_ptr as usize + i * 4, &mut buf).expect("read f32");
        *slot = f32::from_le_bytes(buf);
    }
    assert_eq!(out, hardware_wasm::EXPECTED_MATMUL);
}

fn exports_application_contract_diag_mask(path: &Path) {
    let (mut store, instance) = load_instance(path);

    let diag: TypedFunc<(i32,), i32> =
        instance.get_typed_func(&mut store, "ucf_application_contract_diag_mask").expect("ucf_application_contract_diag_mask");
    let memory = instance.get_memory(&mut store, "memory").expect("export memory");
    let out_ptr = 128i32;
    memory.write(&mut store, out_ptr as usize, &0u32.to_le_bytes()).expect("init mask");

    let status = diag.call(&mut store, (out_ptr,)).expect("diag contract");
    assert_eq!(status, 0);

    let mut buf = [0u8; 4];
    memory.read(&store, out_ptr as usize, &mut buf).expect("read mask");
    let mask = u32::from_le_bytes(buf);
    assert_eq!(mask, hardware_wasm::EXPECTED_DIAG_MASK);
}

fn bind_external_buffer_forwards_to_host_import(path: &Path) {
    let (mut store, instance) = load_instance(path);
    let bind: TypedFunc<(i32, i32), i32> = instance.get_typed_func(&mut store, "ucf_bind_external_buffer").expect("ucf_bind_external_buffer");
    assert_eq!(bind.call(&mut store, (4, 9)).expect("bind"), 0);
}

fn color_fill_rgba8_writes_linear_memory(path: &Path) {
    let (mut store, instance) = load_instance(path);
    let fill: TypedFunc<(i32, i32, i32, i32, i32, i32, i32), i32> =
        instance.get_typed_func(&mut store, "ucf_color_fill_rgba8").expect("ucf_color_fill_rgba8");
    let memory = instance.get_memory(&mut store, "memory").expect("export memory");
    let out_ptr: i32 = 1024;
    let status = fill.call(&mut store, (2, 2, 9, 8, 7, 255, out_ptr)).expect("fill");
    assert_eq!(status, 0);
    let mut px = [0u8; 4];
    memory.read(&store, out_ptr as usize, &mut px).expect("read pixel");
    assert_eq!(px, [9, 8, 7, 255]);
}

fn l2_color_fill_webgpu_runs_cpu_then_host(path: &Path) {
    let (mut store, instance) = load_instance(path);
    let l2: TypedFunc<(i32, i32, i32, i32, i32, i32), i32> =
        instance.get_typed_func(&mut store, "ucf_l2_color_fill_webgpu").expect("ucf_l2_color_fill_webgpu");
    let status = l2.call(&mut store, (4, 4, 10, 20, 30, 255)).expect("l2 fill");
    assert_eq!(status, 0);
}

fn l2_raster_clear_webgpu_runs_cpu_then_host(path: &Path) {
    let (mut store, instance) = load_instance(path);
    let l2: TypedFunc<(i32, i32, i32, i32, i32, i32), i32> =
        instance.get_typed_func(&mut store, "ucf_l2_raster_clear_webgpu").expect("ucf_l2_raster_clear_webgpu");
    let status = l2.call(&mut store, (4, 2, 255, 0, 0, 255)).expect("l2 raster");
    assert_eq!(status, 0);
}

fn raster_clear_rgba8_writes_linear_memory(path: &Path) {
    let (mut store, instance) = load_instance(path);
    let raster: TypedFunc<(i32, i32, i32, i32, i32, i32, i32), i32> =
        instance.get_typed_func(&mut store, "ucf_raster_clear_rgba8").expect("ucf_raster_clear_rgba8");
    let memory = instance.get_memory(&mut store, "memory").expect("export memory");
    let out_ptr: i32 = 2048;
    let status = raster.call(&mut store, (4, 2, 255, 0, 0, 255, out_ptr)).expect("raster readback");
    assert_eq!(status, 0);
    let mut px = [0u8; 4];
    memory.read(&store, out_ptr as usize, &mut px).expect("read pixel");
    assert_eq!(px, [255, 0, 0, 255]);
}

fn raster_tri_rgba8_writes_center_yellow(path: &Path) {
    let (mut store, instance) = load_instance(path);
    let tri: TypedFunc<(i32, i32, i32), i32> = instance.get_typed_func(&mut store, "ucf_raster_tri_rgba8").expect("ucf_raster_tri_rgba8");
    let memory = instance.get_memory(&mut store, "memory").expect("export memory");
    let out_ptr: i32 = 4096;
    let status = tri.call(&mut store, (32, 32, out_ptr)).expect("tri raster");
    assert_eq!(status, 0);
    let w = 32usize;
    let mut center = [0u8; 4];
    memory.read(&store, out_ptr as usize + (16 * w + 16) * 4, &mut center).expect("read center");
    assert_eq!(center, [255, 255, 0, 255]);
}

fn l2_raster_tri_webgpu_runs_cpu_then_host(path: &Path) {
    let (mut store, instance) = load_instance(path);
    let l2: TypedFunc<(i32, i32), i32> = instance.get_typed_func(&mut store, "ucf_l2_raster_tri_webgpu").expect("ucf_l2_raster_tri_webgpu");
    let status = l2.call(&mut store, (32, 32)).expect("l2 raster tri");
    assert_eq!(status, 0);
}

fn main() -> ExitCode {
    let Some(path) = wasm_artifact()
    else {
        return ExitCode::SUCCESS;
    };

    let checks: [(&str, fn(&Path)); 9] = [
        ("ucf_application_contract_matmul", exports_application_contract_matmul),
        ("ucf_application_contract_diag_mask", exports_application_contract_diag_mask),
        ("ucf_bind_external_buffer", bind_external_buffer_forwards_to_host_import),
        ("ucf_color_fill_rgba8", color_fill_rgba8_writes_linear_memory),
        ("ucf_l2_color_fill_webgpu", l2_color_fill_webgpu_runs_cpu_then_host),
        ("ucf_l2_raster_clear_webgpu", l2_raster_clear_webgpu_runs_cpu_then_host),
        ("ucf_raster_clear_rgba8", raster_clear_rgba8_writes_linear_memory),
        ("ucf_raster_tri_rgba8", raster_tri_rgba8_writes_center_yellow),
        ("ucf_l2_raster_tri_webgpu", l2_raster_tri_webgpu_runs_cpu_then_host),
    ];

    println!("artifact: {}", path.display());
    for (name, check) in checks {
        println!("- {name}");
        check(&path);
    }
    println!("{} 项 C ABI 契约通过", checks.len());
    ExitCode::SUCCESS
}
