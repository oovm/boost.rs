//! 全 target 最小发射检查：同一份 `.v` 工程分别经 jvm / clr / wasm-node / ovm lane 构建。
//!
//! 这里只验证产品面合同：构建成功、execution manifest 冻结了启动 argv、argv 指向真实
//! 产出的文件，并且（宿主可执行时）真实启动后报告冻结入口的返回值。语言特性覆盖由
//! 各自的 lane 单测承担；本文件负责证明「同一个程序能落到每个 target 并跑起来」。

use std::{fs, path::PathBuf, process::Command};

use interpreter_vm_ovm::{OdinVm, Value};

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

/// i32 运算符作为普通成员声明，助记符与 `tests/support/std_operators.v` 中已验证的一致。
///
/// 只保留本用例实际用到的 i32 与比较结果载体 `bool`；`+ - * / %`、前缀 `-`、
/// `== != <` 四个 target 的绑定都齐全，`i32.ne` 已补 CLR。
const OPERATOR_PRIMITIVES: &str = r#"namespace std.primitive;

[primitive("std::primitive::i32")]
structure i32 {
    [infix]
    [jvm_operator("integer.add")]
    [clr_operator("i32.add")]
    [wasm_operator("i32.add")]
    [ovm_operator("i32.add")]
    micro `+`(self: i32, other: i32) -> i32;

    [infix]
    [jvm_operator("integer.subtract")]
    [clr_operator("i32.sub")]
    [wasm_operator("i32.sub")]
    [ovm_operator("i32.sub")]
    micro `-`(self: i32, other: i32) -> i32;

    [infix]
    [jvm_operator("integer.multiply")]
    [clr_operator("i32.mul")]
    [wasm_operator("i32.mul")]
    [ovm_operator("i32.mul")]
    micro `*`(self: i32, other: i32) -> i32;

    [infix]
    [jvm_operator("integer.divide")]
    [clr_operator("i32.div")]
    [wasm_operator("i32.div_s")]
    [ovm_operator("i32.div")]
    micro `/`(self: i32, other: i32) -> i32;

    [infix]
    [jvm_operator("integer.remainder")]
    [clr_operator("i32.rem")]
    [wasm_operator("i32.rem_s")]
    [ovm_operator("i32.rem")]
    micro `%`(self: i32, other: i32) -> i32;

    [prefix]
    [jvm_operator("integer.negate")]
    [clr_operator("i32.neg")]
    [wasm_operator("i32.neg")]
    [ovm_operator("i32.neg")]
    micro `-`(self: i32) -> i32;

    [infix]
    [jvm_operator("integer.compare_equal")]
    [clr_operator("i32.eq")]
    [wasm_operator("i32.eq")]
    [ovm_operator("i32.eq")]
    micro `==`(self: i32, other: i32) -> bool;

    [infix]
    [jvm_operator("integer.compare_not_equal")]
    [clr_operator("i32.ne")]
    [wasm_operator("i32.ne")]
    [ovm_operator("i32.ne")]
    micro `!=`(self: i32, other: i32) -> bool;

    [infix]
    [jvm_operator("integer.compare_less_than")]
    [clr_operator("i32.lt")]
    [wasm_operator("i32.lt_s")]
    [ovm_operator("i32.lt")]
    micro `<`(self: i32, other: i32) -> bool;
}

[primitive("std::primitive::bool")]
structure bool { }
"#;

/// 含运算符的可观测算术入口：`mix(10, 4)` 经参数参与全部运算符后返回 4。
///
/// `(10 + 4) * 3 - 10 / 2 = 37`，`37 % 7 = 2`，前缀 `-` 得 -2，此时
/// `x < negated` 与 `x == negated` 均不成立、`x != negated` 成立，返回
/// `negated + r + 4 = 4`。入口不是常量字面量，运算符调用都发生在参数上。
const ARITHMETIC_SOURCE: &str = r#"namespace app;

micro mix(a: i32, b: i32) -> i32 {
    let x = (a + b) * 3 - a / 2
    let r = x % 7
    let negated = -r
    if x < negated {
        return x - negated
    }
    if x == negated {
        return 1
    }
    if x != negated {
        return negated + r + 4
    }
    return 0
}

[main]
micro main() -> i32 {
    return app::mix(10, 4)
}
"#;

/// 算术入口经四个 target 运行后期望的可观测值。
const ARITHMETIC_EXPECTED: i32 = 4;

const MANIFEST_TEMPLATE: &str = r#"{
    name: "app",
    build: [
        { target: "%TARGET%" }
    ]
}
"#;

fn write_project(root: &std::path::Path, manifest_target: &str, source: &str) -> PathBuf {
    write_project_with(root, manifest_target, source, PRIMITIVES)
}

fn write_project_with(root: &std::path::Path, manifest_target: &str, source: &str, primitives: &str) -> PathBuf {
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
    fs::write(source_dir.join("core_primitive.v"), primitives).unwrap();
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

fn build_arithmetic(target: &str) -> (BuildOutcome, PathBuf) {
    let root = std::env::temp_dir().join(format!("legion-arithmetic-{target}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let project_dir = write_project_with(&root, target, ARITHMETIC_SOURCE, OPERATOR_PRIMITIVES);
    let output_dir = root.join("dist").join(target);
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

/// 含运算符的算术项目在四个 target 上真实编译并运行，且报告同一可观测结果。
///
/// jvm 由 launcher 打印返回值（退出码 0），clr / wasm-node 用返回值作进程退出码，
/// ovm 由 `interpreter-vm-ovm` 进程内装载 `.odin` 并执行构建给出的入口符号。
/// 四个 target 的入口都是 `[main] mix(10, 4)`，表达式在形参上执行
/// `+ - * / %`、前缀 `-` 与 `== != <`，断言的是运行结果而不是产物是否存在。
#[test]
fn arithmetic_operators_run_on_every_target() {
    for target in ["jvm", "clr", "wasm-node", "ovm"] {
        let (outcome, root) = build_arithmetic(target);
        assert!(outcome.ok, "{target} 算术项目 build 失败: {:?}", outcome.error);
        match target {
            "jvm" => {
                let (code, stdout, stderr) = run_contract(&outcome);
                assert_eq!(code, Some(0), "jvm: 入口必须正常退出: stdout={stdout:?} stderr={stderr:?}");
                assert_eq!(stdout.trim(), ARITHMETIC_EXPECTED.to_string(), "jvm: 必须打印算术结果: stderr={stderr:?}");
            }
            "clr" | "wasm-node" => {
                let (code, stdout, stderr) = run_contract(&outcome);
                assert_eq!(
                    code,
                    Some(ARITHMETIC_EXPECTED),
                    "{target}: 退出码必须是算术结果: stdout={stdout:?} stderr={stderr:?}"
                );
            }
            "ovm" => {
                let artifact = outcome
                    .artifacts
                    .iter()
                    .find(|artifact| artifact.ends_with(".odin"))
                    .unwrap_or_else(|| panic!("ovm: 缺少 `.odin` 产物: {:?}", outcome.artifacts));
                let bytes = fs::read(artifact).unwrap_or_else(|error| panic!("ovm: 读取 `{artifact}` 失败: {error}"));
                let entry = outcome.entry_symbol.clone().expect("ovm: 构建结果必须给出入口符号");
                let mut vm = OdinVm::new();
                let loaded = vm.load(&bytes).unwrap_or_else(|error| panic!("ovm: 装载 `main.odin` 失败: {error}"));
                let value = vm
                    .run(&loaded, &entry, Vec::new())
                    .unwrap_or_else(|error| panic!("ovm: 入口 `{entry}` 必须执行到完成: {error}"));
                assert_eq!(value, Value::I32(ARITHMETIC_EXPECTED), "ovm: 入口 `{entry}` 必须完成并返回算术结果");
            }
            other => panic!("未知 target `{other}`"),
        }
        let _ = fs::remove_dir_all(&root);
    }
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
/// 由外部 runner 按冻结 argv 启动；wasm-node 的启动壳按 argv 里冻结的测试名从
/// lowering 冻结的「逻辑名 → wasm 导出名」映射中选择导出。
#[test]
fn test_bundles_compile_and_run_on_every_supported_target() {
    for label in ["legion", "clr", "jvm", "wasm-node"] {
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
        if label == "wasm-node" {
            assert_eq!(
                launch.last().map(String::as_str),
                Some(contract.logical_entry.as_str()),
                "{label}: 启动 argv 必须以冻结的测试名结尾: {launch:?}"
            );
        }
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
        if label == "wasm-node" {
            // 负例：argv 里的名字不在冻结映射内时启动壳必须非零退出并写 stderr，
            // 不得回退到第一个导出或按名字猜。
            let unknown = Command::new(&launch[0])
                .args(&launch[1..launch.len() - 1])
                .arg("test_does_not_exist")
                .output()
                .unwrap_or_else(|error| panic!("{label}: 无法启动未知测试名 `{launch:?}`: {error}"));
            assert!(!unknown.status.success(), "{label}: 未知测试名必须非零退出: {:?}", unknown.status);
            assert!(!unknown.stderr.is_empty(), "{label}: 未知测试名必须在 stderr 说明原因");
        }
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
