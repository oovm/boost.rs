//! Architecture guards: the GC substrate must stay language-agnostic.
//!
//! 校验一律写成结构规则：本 crate 只允许引用 `std` / `core` / `alloc` 与自身模块，
//! 清单不得声明任何外部或语言 crate 依赖。

use std::{
    fs,
    path::{Path, PathBuf},
};

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
fn gc_src_must_not_import_external_crates() {
    let mut files = Vec::new();
    collect_rs_files(&crate_src(), &mut files);
    assert!(!files.is_empty(), "interpreter-gc src must exist");
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
                assert!(matches!(root, "crate" | "std" | "core" | "alloc"), "{} imports external crate `{root}`", file.display());
            }
        }
    }
}

#[test]
fn gc_manifest_must_not_depend_on_external_crates() {
    let manifest = fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).expect("read interpreter-gc Cargo.toml");
    let mut in_dependencies = false;
    let mut offenders: Vec<String> = Vec::new();
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_dependencies = trimmed == "[dependencies]" || trimmed.starts_with("[dependencies.");
            continue;
        }
        if in_dependencies && !trimmed.is_empty() && !trimmed.starts_with('#') {
            offenders.push(trimmed.to_owned());
        }
    }
    assert!(offenders.is_empty(), "interpreter-gc Cargo.toml must declare no runtime dependencies, found: {offenders:?}");
}
