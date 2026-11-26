//! 双语言对抗接口门（架构门 N6）。
//!
//! 证明：TS→Futamura→OVM 与 Valkyrie→OVM 是两条**独立前端**链——
//! 不共享语言 HIR/MIR、不共享 provider——但最后汇入同一套 OVM 装配/编码
//! （`compiler.emitter` ovm lane + `data-binary-odin`）与同一 OdinVM
//! loader/verifier/runtime。一方未注册对方的 provider，另一方仍必须正常
//! 装载执行；交换 provider 不得改变对方产物语义。
//!
//! 运行：`cargo test -p package-manager-legion`

use std::{fs, path::PathBuf};

use interpreter_vm_ovm::{OdinVm, Value};
use language_typescript::runtime::provider::TsStdProvider;
use language_typescript::specialize_to_odin;
use package_manager_legion::product::{BuildOptions, BuildOutcome, build_project};

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

/// Valkyrie 侧动态语义案例：条件分支 + 加法（与 TS 案例同构的动态真值/算术）。
const VALKYRIE_SOURCE: &str = r#"namespace app;

micro base(x: i32) -> i32 {
    return x + 1
}

[main]
micro main(flag: i32) -> i32 {
    let v = app::base(flag)
    if v == 42 {
        return v + 100
    }
    return v
}
"#;

const MANIFEST: &str = r#"{
    name: "app",
    build: [
        { target: "ovm" }
    ]
}
"#;

/// TS 侧动态语义案例：truthiness 分支 + 动态文本拼接 + console 副作用。
const TS_SOURCE: &str = r#"
export function greet(input: { name: string, loud: boolean }): string {
    let label = "hi:" + input.name;
    console.log(label);
    if (input.loud) {
        label = label + "!";
    }
    return label;
}
"#;

fn build_valkyrie(case: &str) -> (BuildOutcome, PathBuf) {
    let root = std::env::temp_dir().join(format!("dual-gate-vk-{case}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let source_dir = root.join("source");
    fs::create_dir_all(&source_dir).unwrap();
    fs::write(root.join("legions.von"), "{\n    name: \"dual-gate\",\n    members: []\n}\n").unwrap();
    fs::write(root.join("legion.von"), MANIFEST).unwrap();
    fs::write(source_dir.join("main.v"), VALKYRIE_SOURCE).unwrap();
    fs::write(source_dir.join("core_primitive.v"), PRIMITIVES).unwrap();
    let outcome = build_project(&BuildOptions { project_dir: root.clone(), target: Some("ovm".to_string()), output_dir: Some(root.join("dist")) });
    (outcome, root)
}

fn odin_artifact(outcome: &BuildOutcome) -> PathBuf {
    outcome.artifacts.iter().find(|artifact| artifact.ends_with(".odin")).unwrap_or_else(|| panic!("必须产出 `.odin`: {:?}", outcome.artifacts)).clone().into()
}

/// 双语言产物在同一 VM 实例装载执行：共用 loader/verifier/runtime。
#[test]
fn dual_language_artifacts_share_one_vm() {
    let (outcome, root) = build_valkyrie("share");
    assert!(outcome.ok, "Valkyrie 构建失败: {:?}", outcome.error);
    let vk_bytes = fs::read(odin_artifact(&outcome)).expect("读取 .odin");
    let vk_entry = outcome.entry_symbol.clone().expect("入口符号");

    let ts_artifact = specialize_to_odin(TS_SOURCE, "ts.dual").expect("TS 第一投影");

    // 同一个 OdinVm 实例注册 TS provider；Valkyrie 产物不需要任何语言 provider。
    let mut vm = OdinVm::with_providers(vec![std::sync::Arc::new(TsStdProvider::new(Default::default()))]);
    let vk_loaded = vm.load(&vk_bytes).expect("同一 VM 必须装载 Valkyrie 产物");
    let ts_loaded = vm.load(&ts_artifact.bytes).expect("同一 VM 必须装载 TS 产物");

    // Valkyrie：main(41) → base=42 → 分支命中 → 142。
    let vk_result = vm.run(&vk_loaded, &vk_entry, vec![Value::I32(41)]).expect("Valkyrie 产物必须执行");
    assert_eq!(vk_result, Value::I32(142));
    // 动态分支残差：flag=0 → base=1 → 不命中 → 1。
    let vk_miss = vm.run(&vk_loaded, &vk_entry, vec![Value::I32(0)]).expect("Valkyrie 分支残差必须执行");
    assert_eq!(vk_miss, Value::I32(1));

    // TS：构造 guest 对象并执行。
    let input_layout = ts_artifact
        .layout_fields
        .iter()
        .position(|names| names == &["loud".to_string(), "name".to_string()])
        .expect("输入对象布局必须登记");
    let object = {
        let heap = vm.heap_mut();
        let object = heap.alloc_layout_object(input_layout as u32, 2);
        let name_slot = ts_artifact.layout_fields[input_layout].iter().position(|n| n == "name").unwrap();
        let loud_slot = ts_artifact.layout_fields[input_layout].iter().position(|n| n == "loud").unwrap();
        heap.set_field(object, name_slot, Value::String("vm".to_string())).unwrap();
        heap.set_field(object, loud_slot, Value::Bool(true)).unwrap();
        object
    };
    let ts_result = vm.run(&ts_loaded, "greet", vec![Value::Object(object)]).expect("TS 产物必须经注册的 ts.* provider 执行");
    match &ts_result {
        Value::String(text) => assert_eq!(text, "hi:vm!"),
        other => panic!("greet 必须返回字符串，得到 {other:?}"),
    }
    let _ = fs::remove_dir_all(&root);
}

/// provider 隔离：TS 产物缺 `ts.*` provider 必须装载失败闭合；
/// Valkyrie 产物不受 TS provider 注册影响，结果逐位一致。
#[test]
fn provider_isolation_between_languages() {
    let (outcome, root) = build_valkyrie("isolation");
    assert!(outcome.ok, "Valkyrie 构建失败: {:?}", outcome.error);
    let vk_bytes = fs::read(odin_artifact(&outcome)).expect("读取 .odin");
    let vk_entry = outcome.entry_symbol.clone().expect("入口符号");
    let ts_artifact = specialize_to_odin(TS_SOURCE, "ts.dual").expect("TS 第一投影");

    // 无 provider VM：TS 失败闭合、Valkyrie 照常执行。
    let mut bare = OdinVm::new();
    let error = bare.load(&ts_artifact.bytes).expect_err("缺 ts.* provider 装载 TS 产物必须失败");
    let message = error.to_string();
    assert!(message.contains("unknown host import") && message.contains("ts."), "失败必须点名 ts.* 缺项: {message}");
    let vk_loaded = bare.load(&vk_bytes).expect("Valkyrie 产物不依赖任何语言 provider");
    let bare_result = bare.run(&vk_loaded, &vk_entry, vec![Value::I32(41)]).expect("裸 VM 必须执行 Valkyrie 产物");

    // 注册 TS provider 的 VM：Valkyrie 结果必须逐位一致（provider 不扰动对方语义）。
    let mut with_ts = OdinVm::with_providers(vec![std::sync::Arc::new(TsStdProvider::new(Default::default()))]);
    let vk_loaded2 = with_ts.load(&vk_bytes).expect("注册 TS provider 后 Valkyrie 装载必须照常");
    let ts_result2 = with_ts.run(&vk_loaded2, &vk_entry, vec![Value::I32(41)]).expect("带 TS provider 的 Valkyrie 执行必须照常");
    assert_eq!(bare_result, ts_result2, "TS provider 注册不得改变 Valkyrie 产物语义");
    let _ = fs::remove_dir_all(&root);
}

/// 共同平台机制：两种语言的产物走同一 `data-binary-odin` 解码合同。
#[test]
fn both_artifacts_decode_with_shared_format() {
    let (outcome, root) = build_valkyrie("format");
    assert!(outcome.ok, "Valkyrie 构建失败: {:?}", outcome.error);
    let vk_bytes = fs::read(odin_artifact(&outcome)).expect("读取 .odin");
    let ts_artifact = specialize_to_odin(TS_SOURCE, "ts.dual").expect("TS 第一投影");

    let vk_module = data_binary_odin::decode_module(&vk_bytes).expect("Valkyrie 产物必须按共用格式解码");
    let ts_module = data_binary_odin::decode_module(&ts_artifact.bytes).expect("TS 产物必须按共用格式解码");
    // 两条链的导入符号空间互不重叠：Valkyrie 只用内核符号，TS 只额外用 ts.*。
    let vk_symbols: Vec<&str> = vk_module.imports.iter().map(|import| import.symbol_name.as_str()).collect();
    let ts_symbols: Vec<&str> = ts_module.imports.iter().map(|import| import.symbol_name.as_str()).collect();
    assert!(vk_symbols.iter().all(|symbol| !symbol.starts_with("ts.")), "Valkyrie 产物不得引用 ts.*: {vk_symbols:?}");
    assert!(ts_symbols.iter().any(|symbol| symbol.starts_with("ts.")), "TS 产物必须经 import contract 声明 ts.* provider: {ts_symbols:?}");
    let _ = fs::remove_dir_all(&root);
}
