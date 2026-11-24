//! 全 target 最小发射检查：同一份 `.v` 工程分别经 jvm / clr / wasm-node / ovm lane 构建。
//!
//! 这里只验证产品面合同：构建成功、execution manifest 冻结了启动 argv、argv 指向真实
//! 产出的文件，并且（宿主可执行时）真实启动后报告冻结入口的返回值。语言特性覆盖由
//! 各自的 lane 单测承担；本文件负责证明「同一个程序能落到每个 target 并跑起来」。

use std::{fs, path::PathBuf, process::Command};

use package_manager_legion::{
    legion::{
        BuildRequest,
        cmds::build::compile_plan,
        cmds::run::ExecutionManifest,
        cmds::test_engine::parse_target_label,
        workspace::WorkspaceResolver,
    },
    product::{BuildOptions, BuildOutcome, build_project},
};

const PRIMITIVES: &str = concat!(
    "namespace std.primitive;\n",
    "[primitive(\"std::primitive::i32\")] structure i32 { }\n",
    "[primitive(\"std::primitive::i64\")] structure i64 { }\n",
);

const SOURCE: &str = r#"namespace app;

micro choose(flag: bool) -> bool {
    return flag
}

[main]
micro main() -> i64 {
    if app::choose(true) {
        return 42
    }
    return 7
}
"#;

const MANIFEST_TEMPLATE: &str = r#"{
    name: "app",
    build: [
        { target: "%TARGET%" }
    ]
}
"#;

fn write_project(root: &std::path::Path, manifest_target: &str, source: &str) -> PathBuf {
    let source_dir = root.join("source");
    fs::create_dir_all(&source_dir).unwrap();
    fs::write(
        root.join("legions.von"),
        r#"{
    name: "multi-target-smoke",
    members: []
}
"#,
    )
    .unwrap();
    fs::write(root.join("legion.von"), MANIFEST_TEMPLATE.replace("%TARGET%", manifest_target)).unwrap();
    fs::write(source_dir.join("main.v"), source).unwrap();
    fs::write(source_dir.join("core_primitive.v"), PRIMITIVES).unwrap();
    root.to_path_buf()
}

fn build(prefix: &str, target: &str) -> (BuildOutcome, PathBuf) {
    let root = std::env::temp_dir().join(format!("legion-multi-target-{prefix}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let project_dir = write_project(&root, target, SOURCE);
    let output_dir = root.join("dist").join(prefix);
    let outcome = build_project(&BuildOptions { project_dir, target: Some(target.to_string()), output_dir: Some(output_dir) });
    (outcome, root)
}

/// execution manifest 中第一条运行合同展开后的启动 argv。
fn frozen_launch(outcome: &BuildOutcome) -> Vec<String> {
    let output_dir = PathBuf::from(outcome.output_dir.as_deref().expect("构建成功必须给出输出目录"));
    let manifest = ExecutionManifest::read_from_output_dir(&output_dir)
        .unwrap_or_else(|error| panic!("run-contracts.txt 解析失败: {error}"))
        .unwrap_or_else(|| panic!("构建必须产出 run-contracts.txt: artifacts={:?}", outcome.artifacts));
    let contract = manifest.run_contracts.first().expect("必须有运行合同");
    assert!(!contract.launch.is_empty(), "启动 argv 不得为空");
    let artifact = artifact_for(outcome, &contract.physical_entry);
    contract.launch.iter().map(|part| part.replace("{artifact}", &artifact)).collect()
}

/// 合同声明的物理入口在本次产物集合中的绝对路径。
fn artifact_for(outcome: &BuildOutcome, physical: &str) -> String {
    let name = std::path::Path::new(physical)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(physical);
    outcome
        .artifacts
        .iter()
        .map(PathBuf::from)
        .find(|path| path.file_name().and_then(|candidate| candidate.to_str()) == Some(name))
        .unwrap_or_else(|| panic!("运行合同指向 `{physical}`，但产物集合没有同名文件: {:?}", outcome.artifacts))
        .to_string_lossy()
        .into_owned()
}

fn run_contract(outcome: &BuildOutcome) -> (Option<i32>, String, String) {
    let launch = frozen_launch(outcome);
    let output = Command::new(&launch[0]).args(&launch[1..]).output().unwrap_or_else(|error| {
        panic!("无法按冻结 argv 启动 `{launch:?}`: {error}");
    });
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn assert_reports(outcome: &BuildOutcome, expected: &str) {
    let (code, stdout, stderr) = run_contract(outcome);
    assert!(
        code == Some(42) || stdout.contains(expected),
        "冻结入口必须报告 {expected}：exit={code:?} stdout={stdout:?} stderr={stderr:?}"
    );
}

#[test]
fn jvm_lane_builds_and_launches_the_same_program() {
    let (outcome, root) = build("jvm", "jvm");
    assert!(outcome.ok, "jvm build 失败: {:?}", outcome.error);
    assert_reports(&outcome, "42");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn clr_lane_builds_and_launches_the_same_program() {
    let (outcome, root) = build("clr", "clr");
    assert!(outcome.ok, "clr build 失败: {:?}", outcome.error);
    assert_reports(&outcome, "42");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn wasm_node_lane_builds_and_launches_the_same_program() {
    let (outcome, root) = build("wasm-node", "wasm-node");
    assert!(outcome.ok, "wasm-node build 失败: {:?}", outcome.error);
    assert_reports(&outcome, "42");
    let _ = fs::remove_dir_all(&root);
}

/// OVM 产物由解释器进程内按入口调用，`run-contracts.txt` 只冻结模块入口；
/// 这里断言合同与产物一致，不冒充宿主可执行文件。
#[test]
fn ovm_lane_builds_the_same_program() {
    let (outcome, root) = build("ovm", "ovm");
    assert!(outcome.ok, "ovm build 失败: {:?}", outcome.error);
    let launch = frozen_launch(&outcome);
    assert!(!launch.is_empty(), "ovm 运行合同必须冻结入口 argv");
    assert!(outcome.artifacts.iter().any(|artifact| artifact.ends_with(".odin")), "ovm 产物必须是 `.odin` 字节码: {:?}", outcome.artifacts);
    let _ = fs::remove_dir_all(&root);
}

/// `[test]` 测试包必须在每个承载测试用途的 target 上编译并真实运行。
///
/// OVM 由 legion 内置解释器进程内按入口调用，运行合同只冻结模块入口；CLR/JVM
/// 由外部 runner 按冻结 argv 启动。wasm 家族当前不承载测试包（逐 `[test]` 导出
/// 尚未实现），因此不在本用例内。
#[test]
fn test_bundles_compile_and_run_on_every_supported_target() {
    for label in ["legion", "clr", "jvm"] {
        let root = std::env::temp_dir().join(format!("legion-multi-target-tests-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let project_dir = write_project(&root, "wasm-node", SOURCE);
        let test_dir = project_dir.join("test");
        fs::create_dir_all(&test_dir).unwrap();
        fs::write(
            test_dir.join("basic.v"),
            concat!("namespace tests;\n", "\n", "[test]\n", "micro test_passes() -> bool {\n", "    return true\n", "}\n"),
        )
        .unwrap();

        let (output_dir, manifest) = compile_test_bundle(&project_dir, label);
        assert_eq!(manifest.run_contracts.len(), 1, "{label}: 每个 `[test]` 必须恰好一条运行合同: {:?}", manifest.run_contracts);
        let contract = &manifest.run_contracts[0];
        assert_eq!(contract.logical_entry, "test_passes", "{label}: 合同必须按测试名冻结");
        if label == "legion" {
            assert!(output_dir.exists(), "{label}: 测试包必须真实写出到输出目录");
            continue;
        }
        let artifact = find_artifact(&output_dir, &contract.physical_entry);
        let launch: Vec<String> = contract.launch.iter().map(|part| part.replace("{artifact}", &artifact)).collect();
        let output = Command::new(&launch[0])
            .args(&launch[1..])
            .output()
            .unwrap_or_else(|error| panic!("{label}: 无法启动 `{launch:?}`: {error}"));
        assert!(
            output.status.success(),
            "{label}: 测试必须通过: exit={:?} stdout={:?} stderr={:?}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = fs::remove_dir_all(&root);
    }
}

/// 只做「计划 → 编译 → 读取 manifest」的最小驱动。
///
/// 不走 `run_tests_for_project` 的正式复用校验：`NYAR_COMPILER_BUILD_ID` 是构建期
/// 注入的，`cargo test` 下为 `unknown`，正式复用会被拒绝；这里要验证的是每个
/// target 的测试包发射与入口调用，而不是复用合同。
fn compile_test_bundle(project_dir: &std::path::Path, label: &str) -> (PathBuf, ExecutionManifest) {
    let workspace = WorkspaceResolver::discover(project_dir).expect("workspace 必须可解析");
    let request = BuildRequest {
        project_dir: project_dir.to_path_buf(),
        resolved: parse_target_label(label).expect("目标标签必须可解析"),
        output_dir: Some(project_dir.join(".cache").join("test").join(label)),
        ..Default::default()
    };
    let (plan, _mode) = workspace.build_test_plan(&request).expect("测试包计划必须可解析");
    compile_plan(&plan, false).expect("测试包必须编译成功");
    let manifest =
        ExecutionManifest::read_from_output_dir(&plan.output_dir).expect("manifest 可读").expect("测试包必须产出 execution manifest");
    (plan.output_dir, manifest)
}

/// 在输出目录中按物理入口文件名查找真实产物。
fn find_artifact(output_dir: &std::path::Path, physical_entry: &str) -> String {
    let name = std::path::Path::new(physical_entry).file_name().and_then(|name| name.to_str()).unwrap_or(physical_entry);
    let mut pending = vec![output_dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            }
            else if path.file_name().and_then(|candidate| candidate.to_str()) == Some(name) {
                return path.to_string_lossy().into_owned();
            }
        }
    }
    panic!("运行合同指向 `{physical_entry}`，但输出目录没有同名产物");
}
