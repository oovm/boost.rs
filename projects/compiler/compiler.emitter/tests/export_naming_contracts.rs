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
        lower_fragment_to_machine_library,
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
