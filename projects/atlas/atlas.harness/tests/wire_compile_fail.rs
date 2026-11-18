//! W5：rustc 级 compile-fail 夹具。
//!
//! 每个夹具写成一个独立临时 crate，经 `cargo check --offline` 真编译一次，断言失败且诊断
//! 命中预期文案。使用独立 `CARGO_TARGET_DIR`，避免与外层 `cargo test` 争抢目录锁；依赖只
//! 有本仓 `atlas` 门面，因此离线解析不依赖网络。

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// (夹具名, crate 源码, 期望出现在 stderr 的诊断片段)
const FIXTURES: &[(&str, &str, &str)] = &[
    (
        "missing_input",
        r#"
use atlas::Component;

#[derive(Component)]
#[atlas(runtime = atlas::wire)]
struct Service {
    region: String,
}

fn main() {}
"#,
        "`input` 必填",
    ),
    (
        "optional_on_non_option",
        r#"
use atlas::Component;

#[derive(Component)]
#[atlas(input = ServiceInput, runtime = atlas::wire)]
struct Service {
    #[wire(optional)]
    region: String,
}

fn main() {}
"#,
        "必须是 `Option<T>`",
    ),
    (
        "enum_rejected",
        r#"
use atlas::Component;

#[derive(Component)]
#[atlas(input = ServiceInput, runtime = atlas::wire)]
enum Service {
    A,
}

fn main() {}
"#,
        "只能派生在具名结构体上",
    ),
    (
        "unknown_wire_parameter",
        r#"
use atlas::Component;

#[derive(Component)]
#[atlas(input = ServiceInput, runtime = atlas::wire)]
struct Service {
    #[wire(name = region)]
    region: String,
}

fn main() {}
"#,
        "只支持 `key = Path` 与 `optional`",
    ),
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).expect("定位工作区根").to_path_buf()
}

fn atlas_manifest() -> String {
    workspace_root().join("projects/atlas/atlas").to_string_lossy().replace('\\', "/")
}

fn compile_target() -> PathBuf {
    workspace_root().join("target/atlas-compile-fail")
}

fn write_fixture(name: &str, source: &str) -> PathBuf {
    let crate_dir = compile_target().join("fixtures").join(name);
    if crate_dir.exists() {
        fs::remove_dir_all(&crate_dir).expect("清理旧夹具");
    }
    fs::create_dir_all(crate_dir.join("src")).expect("创建夹具目录");
    let manifest = format!(
        r#"[package]
name = "atlas-compile-fail-{name}"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]

[dependencies]
atlas = {{ path = "{}" }}
"#,
        atlas_manifest()
    );
    fs::write(crate_dir.join("Cargo.toml"), manifest).expect("写入夹具清单");
    fs::write(crate_dir.join("src/main.rs"), source).expect("写入夹具源码");
    crate_dir
}

#[test]
fn rejected_components_fail_at_rustc_level() {
    let target = compile_target();
    fs::create_dir_all(&target).expect("创建编译目录");
    for (name, source, expected) in FIXTURES {
        let crate_dir = write_fixture(name, source);
        let output = Command::new(env!("CARGO"))
            .args(["check", "--offline", "--message-format=short"])
            .current_dir(&crate_dir)
            .env("CARGO_TARGET_DIR", &target)
            .env("CARGO_NET_OFFLINE", "true")
            .env_remove("RUSTFLAGS")
            .output()
            .expect("运行 cargo check");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "夹具 `{name}` 必须编译失败\n{stderr}");
        assert!(stderr.contains(expected), "夹具 `{name}` 的诊断必须包含 `{expected}`\n{stderr}");
    }
}
