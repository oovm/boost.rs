//! Architecture guards: the Odin VM substrate must stay language-agnostic.
//!
//! 校验一律写成结构规则：src 只允许引用 `crate` / `std` / `core` / `alloc` 与三个声明依赖
//! （`acorn_odin` 格式合同、`interpreter_gc` 托管堆、`interpreter_jit` JIT 底座）；
//! 清单只允许这三条运行时依赖，不得出现语言前端或宿主运输依赖。

use std::{
    fs,
    path::{Path, PathBuf},
};

const ALLOWED_IMPORT_ROOTS: &[&str] = &["crate", "std", "core", "alloc", "acorn_odin", "interpreter_gc", "interpreter_jit"];

fn crate_src() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
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
fn odin_src_must_only_import_declared_dependencies() {
    let mut files = Vec::new();
    collect_rs_files(&crate_src(), &mut files);
    assert!(!files.is_empty(), "interpreter-vm-odin-vm src must exist");
    for file in files {
        let source = fs::read_to_string(&file).unwrap_or_else(|error| panic!("failed to read {}: {error}", file.display()));
        for line in source.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("extern crate ")
                && !rest.trim_end_matches(';').starts_with("crate")
            {
                panic!("{} must not extern crate `{rest}`", file.display());
            }
            if let Some(rest) = trimmed.strip_prefix("use ") {
                let root = rest.split([':', ' ', '{', '(']).next().unwrap_or("");
                assert!(ALLOWED_IMPORT_ROOTS.contains(&root), "{} imports undeclared crate `{root}`", file.display());
            }
        }
    }
}

#[test]
fn odin_manifest_must_depend_only_on_format_and_substrate_crates() {
    let manifest = fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).expect("read interpreter-vm-odin-vm Cargo.toml");
    let mut in_dependencies = false;
    let mut offenders: Vec<String> = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_dependencies = trimmed == "[dependencies]" || trimmed.starts_with("[dependencies.");
            continue;
        }
        if in_dependencies && !trimmed.is_empty() && !trimmed.starts_with('#') {
            let name = trimmed.split('=').next().unwrap_or("").trim();
            if !matches!(name, "acorn-odin" | "interpreter-gc" | "interpreter-jit") {
                offenders.push(trimmed.to_owned());
            }
        }
    }
    assert!(offenders.is_empty(), "interpreter-vm-odin-vm Cargo.toml declares dependencies outside the allowed substrate set: {offenders:?}");
}
