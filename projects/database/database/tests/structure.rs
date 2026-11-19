//! 结构门：`database.*` 各 crate 的依赖方向必须保持单向。
//!
//! 这两条规则来自 `database.*` 行为闭包合同，违反时下游只会不断把实现搬错层：
//!
//! 1. 任何 `database.*` crate 都不得依赖产品门面（`facades/*` 的包）。
//!    门面消费 `database.*`，反过来依赖会让 server / query / execution
//!    直接吃到某一产品的领域实现。
//! 2. `database-query` 不得依赖 `database-execution`。查询层只产模型、绑定
//!    结果与计划，算子和写约束执行归执行层。
//!
//! 判定全部来自 `cargo metadata` 的真实 package id 与依赖边（含 dev / build /
//! target 依赖与 `rename` 别名），不再手工解析 manifest 文本。

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
};

/// 仓库根 manifest（本 crate 位于 `projects/database/database`）。
fn root_manifest() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("`database` crate lives under `projects/database`")
        .join("Cargo.toml")
}

/// `cargo metadata --no-deps` 的一个 workspace package。
struct Package {
    id: String,
    name: String,
    manifest_path: PathBuf,
    dependencies: Vec<Dependency>,
}

struct Dependency {
    name: String,
    kind: Option<String>,
}

fn metadata() -> Vec<Package> {
    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()))
        .args(["metadata", "--no-deps", "--format-version", "1", "--manifest-path"])
        .arg(root_manifest())
        .output()
        .expect("`cargo metadata` must run for the structure gate");
    assert!(output.status.success(), "`cargo metadata` failed: {}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("`cargo metadata` JSON");
    let mut packages = Vec::new();
    for entry in value["packages"].as_array().expect("packages array") {
        let mut dependencies = Vec::new();
        for dep in entry["dependencies"].as_array().expect("dependencies array") {
            dependencies.push(Dependency {
                name: dep["name"].as_str().expect("dependency name").to_owned(),
                kind: dep["kind"].as_str().map(str::to_owned),
            });
        }
        packages.push(Package {
            id: entry["id"].as_str().expect("package id").to_owned(),
            name: entry["name"].as_str().expect("package name").to_owned(),
            manifest_path: PathBuf::from(entry["manifest_path"].as_str().expect("manifest path")),
            dependencies,
        });
    }
    packages
}

/// 包目录位于 `projects/database/<prefix>` 之下的 workspace package。
fn under<'a>(packages: &'a [Package], prefix: &Path) -> Vec<&'a Package> {
    let boundary = database_root().join(prefix);
    packages.iter().filter(|package| package.manifest_path.parent().is_some_and(|dir| dir.starts_with(&boundary))).collect()
}

/// `projects/database` 目录（本 crate 的祖父目录）。
fn database_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("`database` crate lives under `projects/database`").to_path_buf()
}

#[test]
fn database_crates_never_depend_on_product_facades() {
    let packages = metadata();
    let facades: BTreeSet<String> = under(&packages, Path::new("facades")).into_iter().map(|package| package.name.clone()).collect();
    assert!(!facades.is_empty(), "expected product facades under `{}`", database_root().join("facades").display());

    let mut violations = Vec::new();
    for package in under(&packages, Path::new("")) {
        if facades.contains(&package.name) {
            continue;
        }
        for dependency in &package.dependencies {
            if facades.contains(&dependency.name) {
                violations.push(format!("{} (dev/build={:?}) depends on facade `{}`", package.id, dependency.kind, dependency.name));
            }
        }
    }

    assert!(violations.is_empty(), "`database.*` must consume owners, never product facades:\n{}", violations.join("\n"));
}

#[test]
fn query_never_depends_on_execution() {
    let packages = metadata();
    let query = packages.iter().find(|package| package.name == "database-query").expect("`database-query` is a workspace package");
    assert!(
        query.manifest_path.starts_with(database_root().join("database.query")),
        "`database-query` must live at its owned path, found {}",
        query.manifest_path.display()
    );
    let edge = query.dependencies.iter().find(|dependency| dependency.name == "database-execution");
    assert!(
        edge.is_none(),
        "`database-query` ({}) must plan only: execution depends on query, not the reverse (edge kind {:?})",
        query.id,
        edge.map(|dependency| &dependency.kind)
    );
}
