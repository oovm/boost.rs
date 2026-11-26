//! Architecture guards for emitter crate boundaries.

use std::{fs, path::PathBuf};

fn collect_rs_files(dir: &PathBuf, out: &mut Vec<PathBuf>) {
    if !dir.is_dir() {
        return;
    }
    for entry in fs::read_dir(dir).unwrap_or_else(|error| panic!("failed to read {}: {error}", dir.display())) {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        }
        else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn wasm_mir_lowering_must_not_emit_raw_leaf_opcodes() {
    let mir_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/lowering/backends/wasm/mir");
    let mut files = Vec::new();
    collect_rs_files(&mir_dir, &mut files);
    let forbidden = ["push(0x04)", "push(0x05)", "push(0x0B)", "push(0x88)", "push(0xAC)", "leaf_opcodes"];
    for path in files {
        let source = fs::read_to_string(&path).unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        for pattern in forbidden {
            assert!(
                !source.contains(pattern),
                "{} must emit Wasm leaf opcodes through `data_binary_wasm::WasmOpcode`, not `{pattern}`",
                path.display()
            );
        }
    }
}

/// 负门（N8）：emitter 不得依赖任何语言 crate。
///
/// 依赖方向合同是 `语言 → 目标`，单向。目标侧出现 `language-*` 边即语言前端
/// 泄漏进平台层，必须失败。
#[test]
fn emitter_must_not_depend_on_language_crates() {
    let manifest = fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).expect("read compiler-emitter Cargo.toml");
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("language-") {
            panic!("compiler-emitter Cargo.toml must not declare a language dependency: `{trimmed}`");
        }
    }
}

/// 负门（N8）：通用 operator 词汇层不得裁决源语义类型。
///
/// `target_operator.rs` 只保留与 ISA 无关的载体/配方词汇；`SemanticType → 载体`
/// 的结算归语言 × 目标适配器（`language.valkyrie::pipeline::operator`）。
#[test]
fn target_operator_vocabulary_must_not_adjudicate_semantic_types() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/target_operator.rs");
    let source = fs::read_to_string(&path).unwrap();
    assert!(!source.contains("SemanticType"), "{} must not reference `SemanticType` — carrier adjudication belongs to the language adapter", path.display());
}

/// 负门（N8）：OVM/JVM/CLR/Machine 后端不得保留接收源链接或源语义类型的
/// operator 裁决入口。
///
/// 链接分类（`*_operator_target`）、host provider 符号裁决（`ovm_host_import_target`）
/// 与按声明签名冻结的 wrapper（`freeze_*_operator(`，非 `_binding` 表）已整体迁入
/// `language.valkyrie::pipeline::*::operator`；后端只保留助记符 → 编码配方表与
/// 绑定类型，机械验证可编码性。
#[test]
fn backends_must_not_keep_source_semantic_operator_entries() {
    let forbidden = [
        "fn ovm_operator_target(",
        "fn ovm_host_import_target(",
        "fn freeze_ovm_operator(",
        "fn jvm_operator_target(",
        "fn freeze_jvm_operator(",
        "fn clr_operator_target(",
        "fn freeze_clr_operator(",
        "fn x86_64_operator_target(",
        "fn freeze_x86_64_operator(",
        "fn ovm_int_shape(",
        "fn x64_int_shape(",
        "fn is_local_carrier_type(",
        "fn is_reference_carrier(",
        "fn is_exit_code_type(",
    ];
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/lowering/backends");
    for lane in ["ovm", "jvm", "clr", "machine"] {
        let mut files = Vec::new();
        collect_rs_files(&base.join(lane), &mut files);
        assert!(!files.is_empty(), "backend lane directory {lane} must exist");
        for path in files {
            let source = fs::read_to_string(&path).unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
            for pattern in forbidden {
                assert!(
                    !source.contains(pattern),
                    "{} must not define source-semantic operator entry `{pattern}` — it belongs to the language × target adapter",
                    path.display()
                );
            }
        }
    }
}

/// 负门（N8）：Wasm GC 载体映射只允许 `WasmTypeFact` 物理入口，禁止
/// `SemanticType` 源类型双轨映射复活。
#[test]
fn wasm_gc_carrier_mapping_must_be_physical_only() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/lowering/backends/wasm/gc.rs");
    let source = fs::read_to_string(&path).unwrap();
    for pattern in ["use compiler_target::SemanticType", "&SemanticType", "SemanticType::"] {
        assert!(!source.contains(pattern), "{} must map carriers from `WasmTypeFact` only — no source-type mirror (`{pattern}`)", path.display());
    }
}
