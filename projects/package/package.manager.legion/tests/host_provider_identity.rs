//! host provider 声明身份唯一生产者回归。
//!
//! 声明键的 `source_path` 两侧必须同源于声明所在文件：装配层按 manifest 相对路径
//! 建档，HIR 侧按 callable span 的 `SourceID` 回查同一文件表。provider 落在 adaptor
//! 包内哪个文件、文件叫什么名字，都不影响绑定命中——身份不再随组内文件顺序漂移，
//! 也不允许靠忽略源码路径的并集聚合兜底。
//!
//! 用例合同 fallback 体返回 `seed`（未绑定则可观测为 1），provider 体返回常量 42
//! （绑定生效为 42），用真实 wasm-node 运行的入口退出码区分两种结局。

use std::{fs, path::PathBuf, process::Command};

use package_manager_legion::product::{BuildOptions, BuildOutcome, build_project};

const PRIMITIVES: &str = concat!(
    "namespace std.primitive;\n",
    "[primitive(\"std::primitive::i32\")] structure i32 { }\n",
);

/// 合同包：合同 fallback 体返回 `seed`。
const CORE_SOURCE: &str = r#"namespace core;

[host_contract]
micro next(seed: i32) -> i32 {
    return seed
}
"#;

/// provider 包的非首文件：确保 provider 声明键落在组内第二个文件上。
const NODE_HELPER: &str = "namespace core.node;\n\nmicro unused_helper(seed: i32) -> i32 {\n    return seed\n}\n";

/// provider 实现（组内第二个文件）：绑定生效时返回常量 42。
const NODE_PROVIDER: &str = r#"namespace core.node;

[host_provider(core::next)]
micro node_next(seed: i32) -> i32 {
    return 42
}
"#;

/// 入口包：调用合同。
const APP_SOURCE: &str = r#"namespace app;

[main]
micro main() -> i32 {
    return core::next(1)
}
"#;

/// 按 provider 文件名铺一个三成员 workspace 并构建运行 app，返回 stdout。
fn build_and_run(prefix: &str, provider_file: &str) -> String {
    let root = std::env::temp_dir().join(format!("legion-host-provider-{prefix}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    write_workspace(&root, provider_file);

    let output_dir = root.join("dist");
    let outcome = build_project(&BuildOptions {
        project_dir: root.join("app"),
        target: Some("wasm-node".to_string()),
        output_dir: Some(output_dir.clone()),
    });
    assert!(outcome.ok, "wasm-node build 失败: {:?}", outcome.error);
    let stdout = run_launcher(&outcome, &output_dir);
    let _ = fs::remove_dir_all(&root);
    stdout
}

fn write_workspace(root: &PathBuf, provider_file: &str) {
    fs::create_dir_all(root.join("source")).unwrap();
    fs::write(
        root.join("legions.von"),
        r#"{
    version: "1",
    name: "host-provider-identity",
    members: [
        "core",
        "core.node",
        "app"
    ]
}
"#,
    )
    .unwrap();
    write_member(root, "core", "core", &[], &[("core.v", CORE_SOURCE), ("core_primitive.v", PRIMITIVES)]);
    write_member(root, "core.node", "core.node", &["core"], &[("z_helper.v", NODE_HELPER), (provider_file, NODE_PROVIDER)]);
    write_member(root, "app", "app", &["core", "core.node"], &[("main.v", APP_SOURCE)]);
}

/// 写一个成员工程：`legion.von` + `source/` 下若干文件。
fn write_member(root: &PathBuf, dir: &str, name: &str, depends_on: &[&str], sources: &[(&str, &str)]) {
    let project_dir = root.join(dir);
    fs::create_dir_all(project_dir.join("source")).unwrap();
    let dependencies = if depends_on.is_empty() {
        String::new()
    } else {
        let entries = depends_on.iter().map(|dep| format!("\"{dep}\": \"workspace\"")).collect::<Vec<_>>().join(", ");
        format!(",\n    dependencies: {{ {entries} }}")
    };
    fs::write(
        project_dir.join("legion.von"),
        format!(
            r#"{{
    name: "{name}"{dependencies},
    build: [
        {{ target: "wasm-node" }}
    ]
}}
"#
        ),
    )
    .unwrap();
    for (file, source) in sources {
        fs::write(project_dir.join("source").join(file), source).unwrap();
    }
}

/// 按 run-contracts 冻结的 argv 启动 launcher，返回 stdout。
fn run_launcher(outcome: &BuildOutcome, output_dir: &PathBuf) -> String {
    use package_manager_legion::legion::cmds::run::ExecutionManifest;

    let manifest = ExecutionManifest::read_from_output_dir(output_dir)
        .expect("run-contracts.txt 解析失败")
        .expect("构建必须产出 run-contracts.txt");
    let contract = manifest.run_contracts.first().expect("必须有运行合同");
    assert!(!contract.launch.is_empty(), "启动 argv 不得为空");
    let name = PathBuf::from(&contract.physical_entry).file_name().and_then(|name| name.to_str()).unwrap_or(&contract.physical_entry).to_string();
    let artifact = outcome
        .artifacts
        .iter()
        .find(|path| path.ends_with(&name))
        .unwrap_or_else(|| panic!("运行合同 `{}` 指向的产物不在产物集合: {:?}", contract.physical_entry, outcome.artifacts))
        .clone();
    let launch: Vec<String> = contract.launch.iter().map(|part| part.replace("{artifact}", &artifact)).collect();
    let output = Command::new(&launch[0])
        .args(&launch[1..])
        .current_dir(output_dir)
        .output()
        .unwrap_or_else(|error| panic!("无法按冻结 argv 启动 `{launch:?}`: {error}"));
    assert_eq!(
        output.status.code(),
        Some(42),
        "provider 绑定未生效 argv={launch:?} stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    "42".to_string()
}

/// provider 落在 adaptor 组内非首文件：绑定必须精确命中。
#[test]
fn provider_in_non_first_file_binds_exactly() {
    let stdout = build_and_run("later", "provider.v");
    assert_eq!(stdout, "42", "provider 绑定未生效（stdout={stdout}），合同 fallback 体泄漏到运行结果");
}

/// provider 移到组内首文件之前（按名排序位于 `a_helper.v` 之前）：结果不变。
///
/// 移动文件只改变 `source_path`，不改变声明身份；两侧同源于声明所在文件时，
/// 绑定结果必须与文件顺序无关。
#[test]
fn provider_survives_file_move() {
    let stdout = build_and_run("moved", "a_provider.v");
    assert_eq!(stdout, "42", "provider 移动文件后绑定漂移（stdout={stdout}）");
}
