//! 导出命名合同：语言侧冻结源码名，各目标所有者按自己的命名合同投影。
//!
//! 覆盖机器码 lane 的三条合同与投影碰撞拒绝：
//! - Node 扩展：有注解用注解名，否则源码名自动 camelCase 投影（`move_zeroes` → `moveZeroes`）；
//! - CPython 扩展：公开名就是源码名（忽略注解改名）；
//! - 普通共享库：C ABI 公开名就是源码名（忽略注解改名）；
//! - 投影碰撞（`foo_bar` 与 `fooBar` 同落 `fooBar`）：Node 写出前拒绝，CPython/普通库不受影响。

use std::collections::BTreeMap;

use compiler_emitter::{
    FragmentSubmission,
    contracts::{Block, BlockRef, ExecutableFunction, Operand, Terminator, Value, ValueOrigin, ValueRef},
    emitter_backend_machine::{
        HostModule, MachineHostExport, MachineLibraryExport, X64LoweredExtension, X64LoweredLibrary, X64Platform, X64ProductTarget,
        cpython_module_name, lower_fragment_to_machine_library,
    },
    testing::lower_fragment_to_machine_extension,
};
use compiler_types::{Identifier, QualifiedName};

/// 构造一个 `i64 -> i64` 的恒等导出函数体（宿主 ABI 合同内的最小可发射形状）。
#[allow(deprecated)]
fn identity_function(symbol: &str) -> ExecutableFunction {
    let i64 = compiler_types::SemanticType::Integer64 { signed: true };
    ExecutableFunction {
        symbol: symbol.to_string(),
        return_type: i64.clone(),
        param_types: vec![i64.clone()],
        value_types: BTreeMap::from([(ValueRef(0), i64)]),
        value_layouts: BTreeMap::new(),
        return_layout: None,
        entry: BlockRef(0),
        values: vec![Value { id: ValueRef(0), origin: ValueOrigin::Parameter { index: 0, name: "x".to_string() } }],
        blocks: vec![Block {
            id: BlockRef(0),
            label: "entry".to_string(),
            parameters: vec![ValueRef(0)],
            instructions: Vec::new(),
            terminator: Terminator::Return { value: Some(Operand::Value(ValueRef(0))) },
        }],
    }
}

/// 用源码名表（可选注解名表）构造执行计划；实例按源码名排序分配。
fn plan_with(sources: &[&str], annotations: &[(&str, &str)]) -> compiler_emitter::TargetExecutionPlan {
    let mut sorted = sources.to_vec();
    sorted.sort();
    let functions = sorted
        .iter()
        .map(|name| (QualifiedName::new(vec![Identifier::new(*name)]), identity_function(name)))
        .collect::<BTreeMap<_, _>>();
    let mut plan = compiler_emitter::TargetExecutionPlan::from_functions(functions);
    for (instance, source) in plan.instances().into_iter().zip(sorted.iter()) {
        plan.testing_export_names_mut().insert(instance, (*source).to_string());
    }
    for (source, annotation) in annotations {
        let instance = plan
            .export_names()
            .iter()
            .find(|(_, name)| name.as_str() == *source)
            .map(|(instance, _)| *instance)
            .expect("注解目标实例存在");
        plan.testing_export_annotations_mut().insert(instance, (*annotation).to_string());
    }
    plan
}

fn submission_for(sources: &[&str], annotations: &[(&str, &str)]) -> FragmentSubmission {
    FragmentSubmission::from_backend_plan(plan_with(sources, annotations))
}

/// 从降低结果里取 (源码名, 公开名) 对，按公开名排序比较。
fn pairs_of(exports: &[MachineHostExport]) -> Vec<(String, String)> {
    exports.iter().map(|export| (export.source_name.clone(), export.public_name.clone())).collect()
}

#[test]
fn node_extension_projects_unannotated_source_to_camel_case() {
    let submission = submission_for(&["move_zeroes"], &[]);
    let target = X64ProductTarget::extension(X64Platform::linux_x64(), HostModule::Node);
    let lowered: X64LoweredExtension = lower_fragment_to_machine_extension(&submission, &target, "app").expect("Node 扩展降低成功");
    assert_eq!(pairs_of(&lowered.host_exports), vec![("move_zeroes".to_string(), "moveZeroes".to_string())]);
}

#[test]
fn node_extension_honors_export_annotation() {
    let submission = submission_for(&["two_sum"], &[("two_sum", "twoSum")]);
    let target = X64ProductTarget::extension(X64Platform::linux_x64(), HostModule::Node);
    let lowered = lower_fragment_to_machine_extension(&submission, &target, "app").expect("Node 扩展降低成功");
    assert_eq!(pairs_of(&lowered.host_exports), vec![("two_sum".to_string(), "twoSum".to_string())]);
}

#[test]
fn cpython_extension_exports_source_name_and_ignores_annotation() {
    let submission = submission_for(&["move_zeroes", "two_sum"], &[("two_sum", "twoSum")]);
    let target = X64ProductTarget::extension(X64Platform::linux_x64(), HostModule::Python);
    let lowered = lower_fragment_to_machine_extension(&submission, &target, "app").expect("CPython 扩展降低成功");
    // CPython 的公开面就是 Python 命名：注解改名在本 lane 被忽略。
    assert_eq!(
        pairs_of(&lowered.host_exports),
        vec![("move_zeroes".to_string(), "move_zeroes".to_string()), ("two_sum".to_string(), "two_sum".to_string())]
    );
}

#[test]
fn plain_library_exports_source_name_and_ignores_annotation() {
    let submission = submission_for(&["move_zeroes", "two_sum"], &[("two_sum", "twoSum")]);
    let lowered: X64LoweredLibrary = lower_fragment_to_machine_library(&submission, X64Platform::linux_x64()).expect("普通库降低成功");
    let pairs = lowered.exports.iter().map(|export: &MachineLibraryExport| (export.source_name.clone(), export.public_name.clone())).collect::<Vec<_>>();
    assert_eq!(
        pairs,
        vec![("move_zeroes".to_string(), "move_zeroes".to_string()), ("two_sum".to_string(), "two_sum".to_string())]
    );
}

#[test]
fn node_projection_collision_is_rejected_with_both_source_names() {
    // `foo_bar` 投影为 `fooBar`，与源码名 `fooBar`（首段原样保留）撞同一公开名。
    let submission = submission_for(&["foo_bar", "fooBar"], &[]);
    let target = X64ProductTarget::extension(X64Platform::linux_x64(), HostModule::Node);
    let error = lower_fragment_to_machine_extension(&submission, &target, "app").expect_err("投影碰撞必须写出前拒绝");
    assert!(error.contains("foo_bar"), "诊断应列出源码名 foo_bar：{error}");
    assert!(error.contains("fooBar"), "诊断应列出源码名 fooBar：{error}");
}

#[test]
fn cpython_module_name_normalizes_project_and_drives_entry_symbol() {
    // 工程名保留仓库身份，模块名是确定性的合法标识符：文件名、`PyInit_`、
    // 清单与宿主 `import` 必须共用同一个值。
    assert_eq!(cpython_module_name("leetcode-multiply-strings").unwrap(), "leetcode_multiply_strings");
    assert_eq!(cpython_module_name("app").unwrap(), "app");
    assert_eq!(cpython_module_name("9lives").unwrap(), "_9lives");
    assert!(cpython_module_name("").is_err(), "空工程名无法归一化成合法模块名");

    let submission = submission_for(&["move_zeroes"], &[]);
    let target = X64ProductTarget::extension(X64Platform::linux_x64(), HostModule::Python);
    let lowered = lower_fragment_to_machine_extension(&submission, &target, "leetcode-multiply-strings").expect("CPython 模块名归一化");
    assert_eq!(lowered.module_name.as_deref(), Some("leetcode_multiply_strings"));
    assert_eq!(lowered.entry_symbol, "PyInit_leetcode_multiply_strings");
    assert_eq!(lowered.artifact_extension, ".so");

    let node = X64ProductTarget::extension(X64Platform::linux_x64(), HostModule::Node);
    let node_lowered = lower_fragment_to_machine_extension(&submission, &node, "leetcode-multiply-strings").expect("Node 扩展降低成功");
    assert_eq!(node_lowered.module_name, None, "Node 扩展不使用 CPython 模块名");
}

#[test]
fn collision_sources_pass_cpython_and_plain_library() {
    // CPython/普通库按源码名导出：`foo_bar` 与 `fooBar` 是两个不同公开名，不碰撞。
    let submission = submission_for(&["foo_bar", "fooBar"], &[]);
    let cpython = X64ProductTarget::extension(X64Platform::linux_x64(), HostModule::Python);
    let lowered = lower_fragment_to_machine_extension(&submission, &cpython, "app").expect("CPython 源码名合同不碰撞");
    assert_eq!(
        pairs_of(&lowered.host_exports),
        vec![("fooBar".to_string(), "fooBar".to_string()), ("foo_bar".to_string(), "foo_bar".to_string())]
    );
    let library_submission = submission_for(&["foo_bar", "fooBar"], &[]);
    let library = lower_fragment_to_machine_library(&library_submission, X64Platform::linux_x64()).expect("普通库源码名合同不碰撞");
    assert_eq!(library.exports.len(), 2);
}

#[test]
fn export_public_name_falls_back_to_source_name() {
    // wasm lane 的合同入口：有注解用注解名，否则源码名。
    let plan = plan_with(&["move_zeroes", "two_sum"], &[("two_sum", "twoSum")]);
    let instances = plan.instances();
    let mut pairs = instances
        .iter()
        .map(|instance| (plan.export_names()[instance].clone(), plan.export_public_name(instance).cloned().expect("必有公开名")))
        .collect::<Vec<_>>();
    pairs.sort();
    assert_eq!(pairs, vec![("move_zeroes".to_string(), "move_zeroes".to_string()), ("two_sum".to_string(), "twoSum".to_string())]);
}

/// wasm 入口符号由宿主边界冻结为 `main` / `_start`，不借用 `[export]` 公开名。
#[test]
fn wasm_binary_entry_uses_boundary_symbol_and_rejects_export_collision() {
    use compiler_target::HostProjectionBoundary;

    // 入口名被一个 `[export]` 公开名占用：必须在写产物前失败闭合，否则入口与
    // 导出会写进同一个导出名，产出重复导出名的非法模块。
    let mut collision = plan_with(&["entry_fn", "helper"], &[("helper", "main")]);
    collision.testing_entry_operation(QualifiedName::new(vec![Identifier::new("entry_fn")]));
    let entry = collision.entry_operation().expect("入口已设置");
    collision.testing_export_names_mut().remove(&entry);
    let submission = FragmentSubmission::from_backend_plan(collision);
    let error = compiler_emitter::testing::lower_fragment_to_wasm_module(&submission, HostProjectionBoundary::WasmJsGlue)
        .expect_err("入口名与导出公开名冲突必须拒绝");
    assert!(error.contains("main"), "诊断应指出宿主入口符号：{error}");
    assert!(error.contains("冲突"), "诊断应说明是命名冲突：{error}");

    // 不冲突时入口只导出一次，名字来自边界词汇而不是导出面。
    let mut disjoint = plan_with(&["entry_fn", "helper"], &[]);
    disjoint.testing_entry_operation(QualifiedName::new(vec![Identifier::new("entry_fn")]));
    let entry = disjoint.entry_operation().expect("入口已设置");
    disjoint.testing_export_names_mut().remove(&entry);
    let submission = FragmentSubmission::from_backend_plan(disjoint);
    let (module, _imports) =
        compiler_emitter::testing::lower_fragment_to_wasm_module(&submission, HostProjectionBoundary::WasmJsGlue).expect("wasm 启动模块降低成功");
    let names = data_binary_wasm::parse_export_section(&module).into_iter().map(|item| item.name).collect::<Vec<_>>();
    assert_eq!(names.iter().filter(|name| name.as_str() == "main").count(), 1, "入口只导出一次: {names:?}");
    assert!(names.iter().any(|name| name == "helper"), "导出面仍保留 helper: {names:?}");
}
