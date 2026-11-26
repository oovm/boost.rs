//! TS → 第一投影 → OVM 纵切链：解释器基线、产物装载执行、产物取证、
//! provider 失败闭合、Node 差分与依赖证明。
//!
//! 运行：`cargo test -p language-typescript --features ovm-lane`

#![cfg(feature = "ovm-lane")]
#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};

use data_binary_odin::{OdinConstant, OdinHeadCode, OdinInstructionForm, decode_module};
use interpreter_vm_ovm::{OdinVm, Value as VmValue};
use language_typescript::runtime::provider::TsStdProvider;
use language_typescript::runtime::{Interpreter, Value};
use language_typescript::specialize::SpecializeFailure;
use language_typescript::ovm::OvmArtifact;
use language_typescript::{parse, specialize_to_odin};

/// 门程序：真实 TS 子集（静态分支 + 动态 truthiness + 动态 `+` + 对象身份）。
const GATE_TS: &str = r#"
const MODE = "fast";

export function score(input: { n: number, flag: boolean, tag: string, delta: number | string }): string {
    let base = 0;
    if (MODE === "fast") {
        base = input.n + 1;
    } else {
        base = input.n * 10;
        console.log("slow path");
    }
    let label = "score:" + base;
    console.log(label);
    if (input.flag) {
        label = label + "!";
    }
    label = label + input.delta;
    return label;
}

export function pick(a: { id: number }, b: { id: number }): boolean {
    return a === b;
}
"#;

/// 未知输入（guest 侧三组，覆盖 number / string / 负小数 delta）。
struct Input {
    n: f64,
    flag: bool,
    tag: &'static str,
    delta: Value,
}

fn inputs() -> Vec<Input> {
    vec![
        Input { n: 41.0, flag: true, tag: "t", delta: Value::Number(100.0) },
        Input { n: 7.0, flag: false, tag: "u", delta: Value::string("x") },
        Input { n: 0.0, flag: true, tag: "v", delta: Value::Number(-2.5) },
    ]
}

fn interpreter_baseline() -> (Vec<String>, Vec<String>) {
    let program = parse(GATE_TS).expect("门程序必须解析");
    let mut interpreter = Interpreter::new(&program).expect("门程序必须装载");
    let mut results = Vec::new();
    let mut logs = Vec::new();
    for input in inputs() {
        let object = Value::object_with([
            ("n".to_string(), Value::Number(input.n)),
            ("flag".to_string(), Value::Bool(input.flag)),
            ("tag".to_string(), Value::string(input.tag)),
            ("delta".to_string(), input.delta.clone()),
        ]);
        let value = interpreter.call_exported("score", &[object]).expect("解释器基线必须成功");
        results.push(language_typescript::runtime::semantics::to_string(&value));
        logs.extend(interpreter.trace().into_iter().skip(logs.len()));
    }
    // 对象身份：同引用真，异引用假。
    let a = Value::object_with([("id".to_string(), Value::Number(1.0))]);
    let b = Value::object_with([("id".to_string(), Value::Number(1.0))]);
    let same = interpreter.call_exported("pick", &[a.clone(), a.clone()]).expect("pick 必须成功");
    let diff = interpreter.call_exported("pick", &[a, b]).expect("pick 必须成功");
    assert!(language_typescript::runtime::semantics::strict_eq(&same, &Value::Bool(true)), "同引用必须为真");
    assert!(language_typescript::runtime::semantics::strict_eq(&diff, &Value::Bool(false)), "异引用必须为假");
    (results, logs)
}

/// 产物装载 + provider 执行：返回值与 log 顺序逐输入一致。
fn run_artifact(artifact: &OvmArtifact) -> (Vec<String>, Vec<String>) {
    let trace = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut vm = OdinVm::with_providers(vec![Arc::new(TsStdProvider::new(trace.clone()))]);
    let loaded = vm.load(&artifact.bytes).expect("注册 provider 后装载必须成功");
    // 输入对象布局：字段按名字排序冻结。
    let input_layout = artifact
        .layout_fields
        .iter()
        .position(|names| names == &["delta".to_string(), "flag".to_string(), "n".to_string(), "tag".to_string()])
        .expect("输入对象布局必须登记");
    let id_layout = artifact.layout_fields.iter().position(|names| names == &["id".to_string()]).expect("pick 参数布局必须登记");
    let mut results = Vec::new();
    for input in inputs() {
        let object = {
            let heap = vm.heap_mut();
            let object = heap.alloc_layout_object(input_layout as u32, 4);
            let slot = |name: &str| artifact.layout_fields[input_layout].iter().position(|n| n == name).expect("字段槽");
            heap.set_field(object, slot("n"), VmValue::F64(input.n)).expect("写入 n 槽");
            heap.set_field(object, slot("flag"), VmValue::Bool(input.flag)).expect("写入 flag 槽");
            heap.set_field(object, slot("tag"), VmValue::String(input.tag.to_string())).expect("写入 tag 槽");
            heap.set_field(object, slot("delta"), match input.delta {
                Value::Number(number) => VmValue::F64(number),
                Value::String(text) => VmValue::String(text.to_string()),
                other => panic!("delta 形态异常: {other:?}"),
            })
            .expect("写入 delta 槽");
            object
        };
        let value = vm.run(&loaded, "score", vec![VmValue::Object(object)]).expect("产物执行必须成功");
        results.push(match &value {
            VmValue::String(text) => text.clone(),
            other => panic!("score 必须返回字符串，得到 {other:?}"),
        });
    }
    // pick 身份：同一 ObjectId 真，不同 ObjectId 假。
    let (first, second) = {
        let heap = vm.heap_mut();
        (heap.alloc_layout_object(id_layout as u32, 1), heap.alloc_layout_object(id_layout as u32, 1))
    };
    let same = vm.run(&loaded, "pick", vec![VmValue::Object(first), VmValue::Object(first)]).expect("pick 必须成功");
    let diff = vm.run(&loaded, "pick", vec![VmValue::Object(first), VmValue::Object(second)]).expect("pick 必须成功");
    assert_eq!(same, VmValue::Bool(true));
    assert_eq!(diff, VmValue::Bool(false));
    let logs = trace.lock().expect("trace 锁不中毒").clone();
    (results, logs)
}

#[test]
fn gate_a_interpreter_baseline() {
    let (results, logs) = interpreter_baseline();
    assert_eq!(results, vec!["score:42!100", "score:8x", "score:1!-2.5"]);
    assert_eq!(logs, vec!["score:42", "score:8", "score:1"]);
}

#[test]
fn gate_b_artifact_matches_interpreter_line_by_line() {
    let artifact = specialize_to_odin(GATE_TS, "ts.gate").expect("第一投影必须成功");
    let (vm_results, vm_logs) = run_artifact(&artifact);
    let (ts_results, ts_logs) = interpreter_baseline();
    assert_eq!(vm_results, ts_results, "产物返回值必须逐输入与解释器基线一致");
    assert_eq!(vm_logs, ts_logs, "产物 log 顺序必须逐输入与解释器基线一致");
}

#[test]
fn gate_c_artifact_forensics() {
    let artifact = specialize_to_odin(GATE_TS, "ts.gate").expect("第一投影必须成功");
    let decoded = decode_module(&artifact.bytes).expect("产物必须可解码");
    let strings: Vec<String> = decoded
        .constants
        .iter()
        .filter_map(|constant| match constant {
            OdinConstant::String(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    // 静态分支消去：`slow` 路径的字符串常量不存在。
    assert!(strings.iter().all(|text| !text.contains("slow")), "静态消去的 else 分支不得留下常量: {strings:?}");
    // 静态分支消去：`input.n * 10` 的宿主乘法不得进入导入表。
    let symbols: Vec<&str> = decoded.imports.iter().map(|import| import.symbol_name.as_str()).collect();
    assert!(!symbols.contains(&"f64_mul"), "静态消去的分支不得留下 f64_mul: {symbols:?}");
    // 已知条件折叠：`MODE === "fast"` 的字符串相等也不应残差。
    assert!(!symbols.contains(&"utf8.eq"), "已知条件必须消去，不残差 utf8.eq: {symbols:?}");
    // 动态 flag 分支保留：`"!"` 常量与条件跳转存在。
    assert!(strings.iter().any(|text| text == "!"), "动态 truthiness 分支必须残差: {strings:?}");
    let head_codes = decode_head_codes(&decoded);
    assert!(head_codes.contains(&OdinHeadCode::JumpIfFalse), "残差分支必须留下 JumpIfFalse 头码: {head_codes:?}");
    // 解释器分派消失：动态语义全部走 provider 符号。
    for required in ["ts.log", "ts.num_to_string", "ts.add", "ts.strictEqual"] {
        assert!(symbols.contains(&required), "provider 符号 `{required}` 必须残差进导入表: {symbols:?}");
    }
    // 静态 number 相加走内核 f64_add，不绕 provider。
    assert!(symbols.contains(&"f64_add"), "静态 number `+` 必须走内核 f64_add: {symbols:?}");
}

#[test]
fn gate_d_missing_provider_fails_closed() {
    let artifact = specialize_to_odin(GATE_TS, "ts.gate").expect("第一投影必须成功");
    let vm = OdinVm::new();
    let error = vm.load(&artifact.bytes).expect_err("不注册 provider 装载必须失败");
    let message = error.to_string();
    assert!(message.contains("unknown host import"), "失败必须结构化指向缺项: {message}");
    assert!(message.contains("ts."), "失败必须指向确切的 `ts.*` 缺项: {message}");
}

#[test]
fn gate_e_node_differential() {
    // Node 差分：等价 JS 与解释器 / 产物的返回值和 console.log 行序一致。
    let script = r#"
const MODE = "fast";
function score(input) {
    let base = 0;
    if (MODE === "fast") { base = input.n + 1; } else { base = input.n * 10; console.log("slow path"); }
    let label = "score:" + base;
    console.log(label);
    if (input.flag) { label = label + "!"; }
    label = label + input.delta;
    return label;
}
function pick(a, b) { return a === b; }
const logs = [];
console.log = (...args) => logs.push(args.join(" "));
const inputs = [
    { n: 41, flag: true, tag: "t", delta: 100 },
    { n: 7, flag: false, tag: "u", delta: "x" },
    { n: 0, flag: true, tag: "v", delta: -2.5 },
];
const results = inputs.map((input) => score(input));
const a = { id: 1 };
process.stdout.write(JSON.stringify({ results, logs, pick_same: pick(a, a), pick_diff: pick(a, { id: 1 }) }));
"#;
    let path = std::env::temp_dir().join("ts_ovm_node_diff.js");
    std::fs::write(&path, script).expect("临时 JS 必须可写");
    let output = std::process::Command::new("node").arg(&path).output().expect("Node v24 必须可用");
    assert!(output.status.success(), "node 执行失败: {}", String::from_utf8_lossy(&output.stderr));
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).expect("node 输出必须是 JSON");
    let node_results: Vec<String> = parsed["results"].as_array().expect("results 数组").iter().map(|v| v.as_str().expect("字符串").to_string()).collect();
    let node_logs: Vec<String> = parsed["logs"].as_array().expect("logs 数组").iter().map(|v| v.as_str().expect("字符串").to_string()).collect();
    assert_eq!(parsed["pick_same"], true);
    assert_eq!(parsed["pick_diff"], false);

    let (ts_results, ts_logs) = interpreter_baseline();
    assert_eq!(ts_results, node_results, "解释器基线必须与 Node 一致");
    assert_eq!(ts_logs, node_logs, "解释器 log 行序必须与 Node 一致");

    let artifact = specialize_to_odin(GATE_TS, "ts.gate").expect("第一投影必须成功");
    let (vm_results, vm_logs) = run_artifact(&artifact);
    assert_eq!(vm_results, node_results, "产物返回值必须与 Node 一致");
    assert_eq!(vm_logs, node_logs, "产物 log 行序必须与 Node 一致");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn gate_f_dependency_proof() {
    // TS 不吃 Valkyrie 类型/std，也不引用旧 God IR 形状名。
    let crate_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = std::fs::read_to_string(crate_root.join("Cargo.toml")).expect("Cargo.toml 必须可读");
    let mut sources = vec![manifest.clone()];
    sources.push(std::fs::read_to_string(crate_root.join("src/lib.rs")).expect("lib.rs 必须可读"));
    for directory in ["syntax", "runtime", "specialize", "ovm", "formatter"] {
        let path = crate_root.join("src").join(directory);
        for entry in walkdir(&path) {
            sources.push(std::fs::read_to_string(entry).expect("源码必须可读"));
        }
    }
    let haystack = sources.join("\n");
    for forbidden in [
        "language.valkyrie",
        "language-valkyrie",
        "compiler-types",
        "compiler_types",
        "TargetExecutionPlan",
        "ExecutableFunction",
        "FragmentSubmission",
        "InstructionKind",
    ] {
        assert!(!haystack.contains(forbidden), "依赖证明失败：发现禁止引用 `{forbidden}`");
    }
}

/// 按编码形逐条解码全部函数体的头码序列。
fn decode_head_codes(module: &data_binary_odin::OdinModuleData) -> Vec<OdinHeadCode> {
    let mut codes = Vec::new();
    for function in &module.functions {
        let body = &module.code_bytes[function.code_offset as usize..(function.code_offset + function.code_length) as usize];
        let mut pc = 0usize;
        while pc < body.len() {
            let code = OdinHeadCode::from_u8(body[pc]).expect("产物头码必须合法");
            codes.push(code);
            pc += match code.form() {
                OdinInstructionForm::Plain => 1,
                OdinInstructionForm::Imm1 => 5,
                OdinInstructionForm::Imm2 => 9,
                OdinInstructionForm::Imm3 => 13,
                _ => panic!("产物含 Invalid 编码形"),
            };
        }
    }
    codes
}

fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("目录必须可读") {
            let path = entry.expect("目录项").path();
            if path.is_dir() {
                stack.push(path);
            }
            else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    files
}

#[test]
fn unsupported_construct_fails_structured() {
    // 失败闭合：动态非 number 的 `-` 无 provider 符号可走，必须结构化失败。
    let source = "export function f(a: number | string): number { return a - 1; }";
    let program = parse(source).expect("解析必须成功");
    let error = language_typescript::ovm::specialize_to_odin(&program, "bad").expect_err("必须失败闭合");
    match error {
        SpecializeFailure::Unsupported { construct, detail } => {
            assert!(construct.contains("二元"), "错误必须指出确切缺项: {construct}");
            assert!(detail.contains("number"), "错误必须说明静态类型要求: {detail}");
        }
        other => panic!("期望结构化 Unsupported，得到 {other:?}"),
    }
}
