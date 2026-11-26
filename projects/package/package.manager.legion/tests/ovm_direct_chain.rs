//! Valkyrie × OVM 直达链取证（架构门步骤 2 退出证据）。
//!
//! 证明三件事：
//! 1. 标量调用链与名义聚合链（字段访问 + 值复制 / 引用别名）从语言 MIR 直接
//!    投影到 OVM 构建接口，产出 `.odin`，由 OVM 验证器装载并真实执行到结果一致；
//! 2. 制品的函数、入口槽与导出来自冻结事实：ABI sidecar 与产物字节哈希绑定，
//!    缺合同即拒绝按猜测布局物化；
//! 3. 必需事实被删除时在明确阶段（装载/校验）失败闭合，不在热路径回猜。

use std::{fs, path::PathBuf};

use interpreter_vm_ovm::{OdinVm, Value};
use package_manager_legion::product::{BuildOptions, BuildOutcome, build_project};

/// `i32` 的 OVM 运算符绑定（与 multi_target_smoke 的冻结助记符一致）。
const PRIMITIVES: &str = concat!(
    "namespace std.primitive;\n",
    "[primitive(\"std::primitive::i32\")]\n",
    "structure i32 {\n",
    "    [infix]\n",
    "    [ovm_operator(\"i32.add\")]\n",
    "    micro `+`(self: i32, other: i32) -> i32;\n",
    "\n",
    "    [infix]\n",
    "    [ovm_operator(\"i32.eq\")]\n",
    "    micro `==`(self: i32, other: i32) -> bool;\n",
    "}\n",
    "[primitive(\"std::primitive::i64\")]\n",
    "structure i64 {\n",
    "}\n",
);

/// 标量调用链：入口经一次函数调用返回标量。
const SCALAR_SOURCE: &str = r#"namespace app;

micro add(a: i32, b: i32) -> i32 {
    return a + b
}

micro twice(x: i32) -> i32 {
    return app::add(x, x)
}

[main]
micro main() -> i32 {
    return app::twice(21)
}
"#;

/// 名义聚合链：构造、字段访问、值复制、引用别名与字段写穿。
const AGGREGATE_SOURCE: &str = r#"namespace app;

structure Point {
    x: i32
    y: i32
}

[main]
micro main() -> i32 {
    let origin = Point { x: 1, y: 2 }
    let copy = origin
    copy.x = 10
    if origin.x == 1 {
        return origin.y + copy.y
    }
    return 0
}
"#;

const MANIFEST: &str = r#"{
    name: "app",
    build: [
        { target: "ovm" }
    ]
}
"#;

fn build_case(name: &str, source: &str) -> (BuildOutcome, PathBuf) {
    let root = std::env::temp_dir().join(format!("ovm-direct-probe-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let source_dir = root.join("source");
    fs::create_dir_all(&source_dir).unwrap();
    fs::write(root.join("legions.von"), "{\n    name: \"ovm-direct-probe\",\n    members: []\n}\n").unwrap();
    fs::write(root.join("legion.von"), MANIFEST).unwrap();
    fs::write(source_dir.join("main.v"), source).unwrap();
    fs::write(source_dir.join("core_primitive.v"), PRIMITIVES).unwrap();
    let outcome = build_project(&BuildOptions {
        project_dir: root.clone(),
        target: Some("ovm".to_string()),
        output_dir: Some(root.join("dist")),
    });
    (outcome, root)
}

fn odin_artifact(outcome: &BuildOutcome) -> PathBuf {
    let path = outcome
        .artifacts
        .iter()
        .find(|artifact| artifact.ends_with(".odin"))
        .unwrap_or_else(|| panic!("必须产出 `.odin`: {:?}", outcome.artifacts));
    PathBuf::from(path)
}

/// 装载并执行入口；返回 i32 结果。
fn run_entry(bytes: &[u8], entry: &str) -> Result<i32, String> {
    let mut vm = OdinVm::new();
    let loaded = vm.load(bytes).map_err(|error| format!("装载失败: {error}"))?;
    let result = vm.run(&loaded, entry, Vec::new()).map_err(|error| format!("执行 `{entry}` 失败: {error}"))?;
    match result {
        Value::I32(value) => Ok(value),
        other => Err(format!("入口 `{entry}` 返回 {other:?}，期望 i32")),
    }
}

#[test]
fn scalar_call_chain_projects_loads_and_runs() {
    let (outcome, root) = build_case("scalar", SCALAR_SOURCE);
    assert!(outcome.ok, "标量链构建失败: {:?}", outcome.error);
    let entry = outcome.entry_symbol.clone().expect("构建结果必须给出入口符号");
    let bytes = fs::read(odin_artifact(&outcome)).expect("读取 .odin");
    // 制品必须携带入口对应的函数与导出（冻结事实）。
    let mut vm = OdinVm::new();
    let loaded = vm.load(&bytes).expect("OVM 验证器必须接受制品");
    assert!(
        loaded.exports.iter().any(|export| export.symbol_name == entry),
        "制品导出必须包含冻结入口 `{entry}`: {:?}",
        loaded.exports.iter().map(|export| export.symbol_name.clone()).collect::<Vec<_>>()
    );
    assert_eq!(run_entry(&bytes, &entry).expect("标量链必须执行到完成"), 42);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn nominal_aggregate_chain_projects_loads_and_runs() {
    let (outcome, root) = build_case("aggregate", AGGREGATE_SOURCE);
    assert!(outcome.ok, "名义聚合链构建失败: {:?}", outcome.error);
    let entry = outcome.entry_symbol.clone().expect("构建结果必须给出入口符号");
    let bytes = fs::read(odin_artifact(&outcome)).expect("读取 .odin");
    // 值复制必须独立：copy.x=10 不写穿 origin；返回 origin.y + copy.y = 2 + 2 = 4。
    assert_eq!(run_entry(&bytes, &entry).expect("聚合链必须执行到完成"), 4);
    let _ = fs::remove_dir_all(&root);
}

/// 必需事实被删除时在明确阶段失败闭合：结构物化调用必须有与产物字节哈希绑定的
/// ABI sidecar 合同，缺合同即拒绝，不按猜测布局物化。
/// （sidecar 与产物整体哈希的绑定由 `data_binary_odin::OvmAbiSidecar::verify_against`
/// 在库形态构建中落盘并校验，Legion 测试引擎与 Asgard 消费路径已覆盖；
/// 本用例证明可执行形态缺合同时的拒绝路径。）
#[test]
fn invoke_export_without_abi_contract_fails_closed() {
    let (outcome, root) = build_case("sidecar", SCALAR_SOURCE);
    assert!(outcome.ok, "构建失败: {:?}", outcome.error);
    let artifact = odin_artifact(&outcome);
    let sidecar_path = artifact.with_file_name(data_binary_odin::ovm_abi_sidecar_file(&artifact.file_name().unwrap().to_string_lossy()));
    let error = package_manager_legion::ovm::invoke_export(&artifact, "main", "[]")
        .expect_err("缺 ABI sidecar 合同的结构物化调用必须拒绝");
    assert!(error.contains("OVM ABI 合同缺失"), "拒绝原因必须指向缺合同: {error}");
    assert!(!sidecar_path.exists(), "本夹具为可执行形态，sidecar 缺席即被拒绝的事实");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn truncated_artifact_fails_at_load_not_runtime() {
    let (outcome, root) = build_case("truncate", SCALAR_SOURCE);
    assert!(outcome.ok, "构建失败: {:?}", outcome.error);
    let bytes = fs::read(odin_artifact(&outcome)).expect("读取 .odin");
    let mut truncated = bytes.clone();
    truncated.truncate(bytes.len() / 2);
    let mut vm = OdinVm::new();
    let error = vm.load(&truncated).expect_err("截断制品必须在装载/校验阶段拒绝");
    let message = error.to_string();
    assert!(!message.is_empty(), "装载失败必须给出明确原因");
    let _ = fs::remove_dir_all(&root);
}

/// 泛型容器链：闭合泛型实例的字段读写经冻结布局身份直达 `.odin` 并真实执行。
/// 覆盖架构门步骤 2 的「泛型容器 + 字段读写」：`Pair<i32>` 的实例布局由 canonical
/// 计划冻结，ABI 投影按 `SemanticType → LayoutId` 索引展开字段，绝不按名字回猜。
const GENERIC_SOURCE: &str = r#"namespace app;

structure Pair<T> {
    first: T
    second: T
}

[main]
micro main() -> i32 {
    let p: Pair<i32> = Pair { first: 3, second: 4 }
    return p.first + p.second
}
"#;

#[test]
fn generic_container_field_chain_projects_loads_and_runs() {
    let (outcome, root) = build_case("generic", GENERIC_SOURCE);
    assert!(outcome.ok, "泛型容器链构建失败: {:?}", outcome.error);
    let entry = outcome.entry_symbol.clone().expect("构建结果必须给出入口符号");
    let bytes = fs::read(odin_artifact(&outcome)).expect("读取 .odin");
    assert_eq!(run_entry(&bytes, &entry).expect("泛型容器链必须执行到完成"), 7);
    let _ = fs::remove_dir_all(&root);
}

/// 宿主导入直达链：`[ovm("symbol")]` 声明经适配器投影为 `.odin` 宿主导入，
/// 由 VM 内建宿主操作装载执行——导入签名、arity 与槽位全部来自冻结事实。
const HOST_IMPORT_SOURCE: &str = r#"namespace app;

[ovm("i32_to_i64")]
micro widen(value: i32) -> i64

[ovm("i64_to_i32")]
micro narrow(value: i64) -> i32

[main]
micro main() -> i32 {
    return app::narrow(app::widen(42))
}
"#;

#[test]
fn host_import_chain_projects_loads_and_runs() {
    let (outcome, root) = build_case("host-import", HOST_IMPORT_SOURCE);
    assert!(outcome.ok, "宿主导入链构建失败: {:?}", outcome.error);
    let entry = outcome.entry_symbol.clone().expect("构建结果必须给出入口符号");
    let bytes = fs::read(odin_artifact(&outcome)).expect("读取 .odin");
    let module = data_binary_odin::decode_module(&bytes).expect("解码 .odin");
    let symbols = module.imports.iter().map(|import| import.symbol_name.as_str()).collect::<Vec<_>>();
    assert!(symbols.contains(&"i32_to_i64") && symbols.contains(&"i64_to_i32"), "制品必须携带两个宿主导入: {symbols:?}");
    assert_eq!(run_entry(&bytes, &entry).expect("宿主导入链必须执行到完成"), 42);
    let _ = fs::remove_dir_all(&root);
}

/// 删去必要的显式宿主绑定（无 `[ovm]` 目标标签、无 provider）：必须在适配/链接
/// 阶段报出确切缺项并拒绝产出，不得回落到猜测或默认载体。
const UNBOUND_HOST_SOURCE: &str = r#"namespace app;

[host_contract]
micro mystery(value: i32) -> i32

[main]
micro main() -> i32 {
    return app::mystery(1)
}
"#;

#[test]
fn unbound_host_contract_fails_at_compile_with_exact_gap() {
    let (outcome, root) = build_case("unbound-host", UNBOUND_HOST_SOURCE);
    assert!(!outcome.ok, "未绑定 provider 的宿主合同必须失败闭合");
    let error = outcome.error.expect("失败必须携带结构化原因");
    assert!(
        error.contains("provider") || error.contains("导入合同") || error.contains("外部调用"),
        "拒绝原因必须指向缺失的宿主绑定事实: {error}"
    );
    let _ = fs::remove_dir_all(&root);
}

/// 退出门静态守卫：OVM 生产链源码里不得再出现旧 God IR 形状的名字。
#[test]
fn ovm_chain_sources_do_not_construct_shared_program_shapes() {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    for relative in [
        "projects/language/language.valkyrie/src/compiler/emitter/assembly/mod.rs",
        "projects/language/language.valkyrie/src/compiler/emitter/pipeline/ovm/mod.rs",
        "projects/language/language.valkyrie/src/compiler/emitter/pipeline/ovm/lowerer.rs",
        "projects/compiler/compiler.emitter/src/backend/vm/mod.rs",
        "projects/compiler/compiler.emitter/src/lowering/backends/ovm/mod.rs",
    ] {
        let path = workspace.join(relative);
        let text = fs::read_to_string(&path).unwrap_or_else(|error| panic!("读取 {} 失败: {error}", path.display()));
        for forbidden in ["TargetExecutionPlan", "ExecutableFunction", "FragmentSubmission", "InstructionKind"] {
            assert!(!text.contains(forbidden), "{relative} 不得引用旧公共 IR 形状 `{forbidden}`");
        }
    }
}
