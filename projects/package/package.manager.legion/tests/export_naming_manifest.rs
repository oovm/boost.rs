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

/// 数组形参导出：托管 lane 的入口 ABI 事实不再被 argv 编组能力限制。
/// 入口仍然冻结签名，只是不生成 argv launcher。
const ARRAY_SOURCE: &str = r#"namespace app;

[export]
micro first_or_zero(xs: [i64]) -> i64 {
    return 0
}
"#;

/// 普通共享库的窄整型载体：`i32` 标量与 `[i32]` 数组按结构标签进入清单，
/// 不退化为 `[i32]` 这类 Display 文本。
const NARROW_SOURCE: &str = r#"namespace app;

[export]
micro narrow_pair(x: i32, xs: [i32]) -> i32 {
    return x
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

fn build_library_with_name(prefix: &str, project_name: &str, source: &str, target: &str) -> (BuildOutcome, PathBuf) {
    let manifest_target = if matches!(target, "node" | "python" | "native") { "native" } else { target };
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
    let manifest = MANIFEST_TEMPLATE.replace("%TARGET%", manifest_target).replace("name: \"app\"", &format!("name: \"{project_name}\""));
    fs::write(root.join("legion.von"), manifest).unwrap();
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
    assert!(move_zeroes.signature_available, "C ABI 导出带冻结签名");
    assert_eq!(move_zeroes.parameters, vec!["i64".to_string()], "C ABI 清单记录冻结形参载体");
    assert_eq!(move_zeroes.parameter_names, vec!["x".to_string()], "C ABI 清单记录形参声明名，供按名绑定");
    assert_eq!(move_zeroes.returns, "i64", "C ABI 清单记录冻结返回载体");
    assert!(move_zeroes.module_name.is_empty(), "C ABI 不依赖模块名，清单不填 module_name");
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
    assert!(move_zeroes.signature_available, "Node-API 导出带冻结签名");
    assert_eq!(move_zeroes.parameters, vec!["i64".to_string()], "Node-API 清单记录冻结形参载体");
    assert_eq!(move_zeroes.parameter_names, vec!["x".to_string()], "Node-API 清单记录形参声明名");
    assert_eq!(move_zeroes.returns, "i64", "Node-API 清单记录冻结返回载体");
    assert!(move_zeroes.module_name.is_empty(), "Node-API 不依赖模块名，清单不填 module_name");
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
    assert!(move_zeroes.signature_available, "wasm 导出已带冻结载体签名");
    assert_eq!(move_zeroes.parameters, vec!["i64".to_string()], "wasm 清单记录冻结形参载体");
    assert_eq!(move_zeroes.parameter_names, vec!["x".to_string()], "wasm 清单记录形参声明名");
    assert_eq!(move_zeroes.returns, "i64", "wasm 清单记录冻结返回载体");
    let two_sum = export_of(&manifest, "two_sum");
    assert_eq!(two_sum.logical_name, "twoSum", "wasm 有注解时导出注解名");
    assert_eq!(two_sum.abi, "wasm");
    let _ = fs::remove_dir_all(&root);
}

/// CPython：工程名 `leetcode-multiply-strings` 归一化为可导入模块名，
/// 文件名前缀、`PyInit_` 与清单 `module_name` 同源；导出名保留源码名。
#[cfg(windows)]
#[test]
fn cpython_export_index_freezes_normalized_module_name() {
    let (outcome, root) = build_library_with_name("python", "leetcode-multiply-strings", LIB_SOURCE, "python");
    assert!(outcome.ok, "CPython 扩展构建失败: {:?}", outcome.error);
    let manifest = manifest_at(&outcome);
    let move_zeroes = export_of(&manifest, "move_zeroes");
    assert_eq!(manifest.project_name, "leetcode-multiply-strings", "清单保留题库工程名");
    assert_eq!(move_zeroes.abi, "cpython");
    assert_eq!(move_zeroes.module_name, "leetcode_multiply_strings", "清单冻结归一化模块名");
    assert!(
        move_zeroes.physical_artifact.starts_with("leetcode_multiply_strings."),
        "产物文件名以同一模块名为前缀: {}",
        move_zeroes.physical_artifact
    );
    assert_eq!(move_zeroes.logical_name, "move_zeroes", "CPython 保留源码名，不自动 snake_case");
    assert!(move_zeroes.signature_available);
    assert_eq!(move_zeroes.parameters, vec!["i64".to_string()]);
    assert_eq!(move_zeroes.parameter_names, vec!["x".to_string()]);
    let two_sum = export_of(&manifest, "two_sum");
    assert_eq!(two_sum.logical_name, "two_sum", "CPython 忽略 `case:` 注解");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn native_library_export_index_projects_lane_carrier_vocabulary() {
    let (outcome, root) = build_library("native-carrier", NARROW_SOURCE, "native");
    assert!(outcome.ok, "native 库构建失败: {:?}", outcome.error);
    let manifest = manifest_at(&outcome);
    let export = export_of(&manifest, "narrow_pair");
    assert_eq!(export.abi, "c-abi");
    assert_eq!(export.parameters, vec!["i32".to_string(), "i32[]".to_string()], "native 清单按精确语言类型与结构数组标签记录形参载体");
    assert_eq!(export.parameter_names, vec!["x".to_string(), "xs".to_string()], "native 清单按声明顺序记录形参名");
    assert_eq!(export.returns, "i32");
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

#[test]
fn jvm_library_export_index_projects_managed_signature() {
    let (outcome, root) = build_library("jvm", LIB_SOURCE, "jvm");
    assert!(outcome.ok, "JVM 库构建失败: {:?}", outcome.error);
    let manifest = manifest_at(&outcome);
    let move_zeroes = export_of(&manifest, "move_zeroes");
    assert_eq!(move_zeroes.abi, "jvm-managed");
    assert!(move_zeroes.signature_available, "JVM managed ABI 导出带冻结签名");
    assert_eq!(move_zeroes.parameters, vec!["i64".to_string()], "JVM 清单记录冻结形参载体");
    assert_eq!(move_zeroes.parameter_names, vec!["x".to_string()], "JVM 清单记录形参声明名");
    assert_eq!(move_zeroes.returns, "i64", "JVM 清单记录冻结返回载体");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clr_library_export_index_projects_managed_signature() {
    let (outcome, root) = build_library("clr", LIB_SOURCE, "clr");
    assert!(outcome.ok, "CLR 库构建失败: {:?}", outcome.error);
    let manifest = manifest_at(&outcome);
    let move_zeroes = export_of(&manifest, "move_zeroes");
    assert_eq!(move_zeroes.abi, "clr-managed");
    assert!(move_zeroes.signature_available, "CLR managed ABI 导出带冻结签名");
    assert_eq!(move_zeroes.parameters, vec!["i64".to_string()], "CLR 清单记录冻结形参载体");
    assert_eq!(move_zeroes.parameter_names, vec!["x".to_string()], "CLR 清单记录形参声明名");
    assert_eq!(move_zeroes.returns, "i64", "CLR 清单记录冻结返回载体");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn jvm_array_parameter_export_projects_frozen_carrier_without_argv_launcher() {
    let (outcome, root) = build_library("jvm-array", ARRAY_SOURCE, "jvm");
    assert!(outcome.ok, "JVM 数组形参库构建失败: {:?}", outcome.error);
    let manifest = manifest_at(&outcome);
    let export = export_of(&manifest, "first_or_zero");
    assert_eq!(export.abi, "jvm-managed");
    assert!(export.signature_available, "数组形参入口仍是合法库 ABI 事实");
    assert_eq!(export.parameters, vec!["i64[]".to_string()], "JVM 清单冻结数组载体");
    assert_eq!(export.parameter_names, vec!["xs".to_string()], "JVM 清单记录形参声明名");
    assert_eq!(export.returns, "i64");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clr_array_parameter_export_projects_frozen_carrier_without_argv_launcher() {
    let (outcome, root) = build_library("clr-array", ARRAY_SOURCE, "clr");
    assert!(outcome.ok, "CLR 数组形参库构建失败: {:?}", outcome.error);
    let manifest = manifest_at(&outcome);
    let export = export_of(&manifest, "first_or_zero");
    assert_eq!(export.abi, "clr-managed");
    assert!(export.signature_available, "数组形参入口仍是合法库 ABI 事实");
    assert_eq!(export.parameters, vec!["i64[]".to_string()], "CLR 清单冻结数组载体");
    assert_eq!(export.parameter_names, vec!["xs".to_string()], "CLR 清单记录形参声明名");
    assert_eq!(export.returns, "i64");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn ovm_library_export_index_projects_contract_signature() {
    let (outcome, root) = build_library("ovm", LIB_SOURCE, "ovm");
    assert!(outcome.ok, "OVM 库构建失败: {:?}", outcome.error);
    let manifest = manifest_at(&outcome);
    let move_zeroes = export_of(&manifest, "move_zeroes");
    assert_eq!(move_zeroes.abi, "ovm");
    assert!(move_zeroes.signature_available, "OVM ABI 合同导出带冻结签名");
    assert_eq!(move_zeroes.parameters, vec!["i64".to_string()], "OVM 清单记录冻结形参载体");
    assert_eq!(move_zeroes.parameter_names, vec!["x".to_string()], "OVM 清单记录形参声明名");
    assert_eq!(move_zeroes.returns, "i64", "OVM 清单记录冻结返回载体");
    let _ = fs::remove_dir_all(&root);
}
