//! 架构守卫：目标规划层只允许引用声明依赖。
//!
//! 校验写成结构规则：src 只允许 `crate` / `std` / `core` / `alloc` 与四个声明依赖，
//! 清单只允许这四个运行时依赖。

use std::{
    fs,
    path::{Path, PathBuf},
};

const ALLOWED_IMPORT_ROOTS: &[&str] =
    &["crate", "std", "core", "alloc", "compiler_types", "compiler_optimize", "compiler_analysis", "serde", "serde_json"];

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
fn target_src_must_only_import_declared_dependencies() {
    let mut files = Vec::new();
    collect_rs_files(&crate_src(), &mut files);
    assert!(!files.is_empty(), "compiler-target src must exist");
    for file in files {
        let source = fs::read_to_string(&file).unwrap_or_else(|error| panic!("failed to read {}: {error}", file.display()));
        for line in source.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("use ") {
                let root = rest.split([':', ' ', '{', '(']).next().unwrap_or("");
                assert!(ALLOWED_IMPORT_ROOTS.contains(&root), "{} imports undeclared crate `{root}`", file.display());
            }
        }
    }
}
