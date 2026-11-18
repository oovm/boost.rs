//! 结构门：`database.*` 各 crate 的依赖方向必须保持单向。
//!
//! 这两条规则来自 `database.*` 行为闭包合同，违反时下游只会不断把实现搬错层：
//!
//! 1. 任何 `database.*` crate 都不得依赖产品门面（`facades/*` 的包）。
//!    门面消费 `database.*`，反过来依赖会让 server / query / execution
//!    直接吃到某一产品的领域实现。
//! 2. `database-query` 不得依赖 `database-execution`。查询层只产模型、绑定
//!    结果与计划，算子和写约束执行归执行层。

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

/// `projects/database` 目录（本 crate 的祖父目录）。
fn database_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("`database` crate lives under `projects/database`")
        .to_path_buf()
}

/// 一个 manifest 的全部依赖键（含 dev / build / target 条件表）。
fn dependency_keys(manifest: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let mut in_dependencies = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let section = &trimmed[1..trimmed.len() - 1];
            in_dependencies =
                section.ends_with("dependencies") && !section.starts_with("workspace");
            continue;
        }
        if !in_dependencies || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, _)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_end_matches(".workspace").trim();
        if !key.is_empty() && !key.contains(char::is_whitespace) {
            keys.insert(key.to_owned());
        }
    }
    keys
}

/// `[package] name` 声明的包名。
fn package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_package = trimmed == "[package]";
            continue;
        }
        if in_package && let Some((key, value)) = trimmed.split_once('=') {
            if key.trim() == "name" {
                return Some(value.trim().trim_matches('"').to_owned());
            }
        }
    }
    None
}

/// `facades/*` 下所有包名。
fn facade_names(root: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let Ok(entries) = fs::read_dir(root.join("facades")) else {
        return names;
    };
    for entry in entries.flatten() {
        let Ok(manifest) = fs::read_to_string(entry.path().join("Cargo.toml")) else {
            continue;
        };
        if let Some(name) = package_name(&manifest) {
            names.insert(name);
        }
    }
    names
}

/// `projects/database/<crate>/Cargo.toml`（不含 `facades`）。
fn database_manifests(root: &Path) -> Vec<(PathBuf, String)> {
    let mut manifests = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return manifests;
    };
    for entry in entries.flatten() {
        let manifest = entry.path().join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        if let Ok(text) = fs::read_to_string(&manifest) {
            manifests.push((manifest, text));
        }
    }
    manifests
}

#[test]
fn database_crates_never_depend_on_product_facades() {
    let root = database_root();
    let facades = facade_names(&root);
    assert!(
        !facades.is_empty(),
        "expected product facades under `{}`",
        root.join("facades").display()
    );

    let mut violations = Vec::new();
    for (manifest, text) in database_manifests(&root) {
        let dependencies = dependency_keys(&text);
        for facade in &facades {
            if dependencies.contains(facade) {
                violations.push(format!("{} depends on facade `{facade}`", manifest.display()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "`database.*` must consume owners, never product facades:\n{}",
        violations.join("\n")
    );
}

#[test]
fn query_never_depends_on_execution() {
    let root = database_root();
    let manifest = root.join("database.query").join("Cargo.toml");
    let text = fs::read_to_string(&manifest).expect("database.query manifest");
    let dependencies = dependency_keys(&text);

    assert!(
        !dependencies.contains("database-execution"),
        "`database-query` must plan only: execution depends on query, not the reverse"
    );
}
