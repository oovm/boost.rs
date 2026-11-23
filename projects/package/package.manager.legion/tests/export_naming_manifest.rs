//! 导出命名合同的端到端验收：语言侧冻结源码名，各目标所有者按自己的命名合同投影，
//! 投影结果与源码名一起进入 `run-contracts.txt` 的 `library_exports`。
//!
//! 覆盖：
//! - 普通 DLL（c-abi）：公开名就是源码名，`case:` 注解在本 lane 被忽略；
//! - Node 扩展（node-api）：无注解源码名自动 camelCase 投影，有注解用注解名；
//! - wasm library：有注解用注解名，无注解用源码名；
//! - Node 投影碰撞（`foo_bar` 与 `fooBar`）：构建失败且诊断同时列出两处源码名，
//!   普通 DLL 同源工程不失败。

use std::{fs, path::PathBuf};

use package_manager_legion::{
    legion::cmds::run::ExecutionManifest,
    product::{BuildOptions, BuildOutcome, build_project},
};

const PRIMITIVES: &str = concat!(
    "namespace std.primitive;\n",
    "[primitive(\"std::primitive::i32\")] structure i32 { }\n",
    "[primitive(\"std::primitive::i64\")] structure i64 { }\n",
);

/// 无注解 `move_zeroes` + `case: "camelCase"` 注解的 `two_sum`。
const LIB_SOURCE: &str = r#"namespace app;

[export]
micro move_zeroes(x: i64) -> i64 {
    return x
}

[export(case: "camelCase")]
micro two_sum(a: i64, b: i64) -> i64 {
    return a
}
"#;

/// `foo_bar` 与 `fooBar` 在 Node camelCase 投影下撞同一公开名。
const COLLISION_SOURCE: &str = r#"namespace app;

[export]
micro foo_bar(x: i64) -> i64 {
    return x
}

[export]
micro fooBar(x: i64) -> i64 {
    return x
}
"#;

/// manifest 的 `build.target` 只能是平台身份；`node` 等宿主合同占位符经
/// `BuildOptions.target`（作者面目标标签）传入。
const MANIFEST_TEMPLATE: &str = r#"{
    name: "app",
    artifact: "library",
    build: [
        { target: "%TARGET%" }
    ]
}
"#;

fn build_library(prefix: &str, source: &str, target: &str) -> (BuildOutcome, PathBuf) {
    // 宿主合同占位符（`node`）不能写进 manifest 的平台身份字段；manifest 用宿主平台，
    // 请求目标携带完整作者面标签。
    let manifest_target = if matches!(target, "node" | "python" | "native") { "native" } else { target };
    build_library_with_manifest_target(prefix, source, manifest_target, target)
}

fn build_library_with_manifest_target(prefix: &str, source: &str, manifest_target: &str, target: &str) -> (BuildOutcome, PathBuf) {
    let root = std::env::temp_dir().join(format!("legion-export-naming-{prefix}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let source_dir = root.join("source");
    fs::create_dir_all(&source_dir).unwrap();
    fs::write(
        root.join("legions.von"),
        r#"{
    name: "export-naming",
    members: []
}
"#,
    )
    .unwrap();
    fs::write(root.join("legion.von"), MANIFEST_TEMPLATE.replace("%TARGET%", manifest_target)).unwrap();
    fs::write(source_dir.join("main.v"), source).unwrap();
    fs::write(source_dir.join("core_primitive.v"), PRIMITIVES).unwrap();
    let output_dir = root.join("dist").join("out");
    let outcome = build_project(&BuildOptions { project_dir: root.clone(), target: Some(target.to_string()), output_dir: Some(output_dir) });
    (outcome, root)
}

fn manifest_at(outcome: &BuildOutcome) -> ExecutionManifest {
    let artifact = PathBuf::from(outcome.artifacts.first().expect("库交付至少有一个产物"));
    let dir = artifact.parent().expect("产物在输出目录内");
    ExecutionManifest::read_from_output_dir(dir).expect("读取 run-contracts.txt").expect("库构建必须写出 execution manifest")
}

fn export_of(manifest: &ExecutionManifest, source_name: &str) -> package_manager_legion::legion::cmds::run::LibraryExportIndexEntry {
    manifest
        .library_exports
        .iter()
        .find(|export| export.source_name == source_name)
        .unwrap_or_else(|| panic!("清单缺少源码名 `{source_name}` 的导出: {:?}", manifest.library_exports))
        .clone()
}

#[cfg(windows)]
#[test]
fn plain_dll_export_index_uses_source_names_and_ignores_case_annotation() {
    let (outcome, root) = build_library("dll", LIB_SOURCE, "x86_64-pc-windows-msvc");
    assert!(outcome.ok, "普通库构建失败: {:?}", outcome.error);
    let manifest = manifest_at(&outcome);
    let move_zeroes = export_of(&manifest, "move_zeroes");
    assert_eq!(move_zeroes.logical_name, "move_zeroes", "C ABI 公开名就是源码名");
    assert_eq!(move_zeroes.abi, "c-abi");
    let two_sum = export_of(&manifest, "two_sum");
    assert_eq!(two_sum.logical_name, "two_sum", "C ABI 忽略 `case:` 注解改名");
    assert_eq!(two_sum.abi, "c-abi");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn node_extension_export_index_projects_camel_case_and_honors_annotation() {
    let (outcome, root) = build_library("node", LIB_SOURCE, "node");
    assert!(outcome.ok, "Node 扩展构建失败: {:?}", outcome.error);
    let manifest = manifest_at(&outcome);
    let move_zeroes = export_of(&manifest, "move_zeroes");
    assert_eq!(move_zeroes.logical_name, "moveZeroes", "Node 无注解时按 camelCase 投影源码名");
    assert_eq!(move_zeroes.abi, "node-api");
    let two_sum = export_of(&manifest, "two_sum");
    assert_eq!(two_sum.logical_name, "twoSum", "Node 有注解时用注解名（兼容旧行为）");
    assert_eq!(two_sum.abi, "node-api");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn wasm_library_export_index_applies_annotation_contract() {
    let (outcome, root) = build_library("wasm", LIB_SOURCE, "wasm32-unknown-unknown");
    assert!(outcome.ok, "wasm 库构建失败: {:?}", outcome.error);
    let manifest = manifest_at(&outcome);
    let move_zeroes = export_of(&manifest, "move_zeroes");
    assert_eq!(move_zeroes.logical_name, "move_zeroes", "wasm 无注解时导出源码名");
    assert_eq!(move_zeroes.abi, "wasm");
    let two_sum = export_of(&manifest, "two_sum");
    assert_eq!(two_sum.logical_name, "twoSum", "wasm 有注解时导出注解名");
    assert_eq!(two_sum.abi, "wasm");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn node_projection_collision_fails_with_both_source_names() {
    let (outcome, root) = build_library("collision-node", COLLISION_SOURCE, "node");
    assert!(!outcome.ok, "投影碰撞必须写出前拒绝: {:?}", outcome.artifacts);
    let error = outcome.error.unwrap_or_default();
    assert!(error.contains("foo_bar"), "诊断应列出源码名 foo_bar：{error}");
    assert!(error.contains("fooBar"), "诊断应列出源码名 fooBar：{error}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn collision_sources_still_build_for_plain_library() {
    // 普通库按源码名导出：`foo_bar` 与 `fooBar` 是两个不同 C ABI 符号，不碰撞。
    let (outcome, root) = build_library("collision-dll", COLLISION_SOURCE, "x86_64-unknown-linux-gnu");
    assert!(outcome.ok, "普通库源码名合同不应碰撞: {:?}", outcome.error);
    let manifest = manifest_at(&outcome);
    assert_eq!(manifest.library_exports.len(), 2);
    let _ = fs::remove_dir_all(&root);
}
