//! 导出命名合同：语言侧冻结源码名，各目标所有者按自己的命名合同投影。
//!
//! 机器码 lane（Node / CPython / 普通共享库）与 wasm lane 的投影、碰撞拒绝已随
//! lowering 迁到语言侧 `language.valkyrie` 的对应管道；本文件保留 CPython 模块名
//! 归一化纯函数与仍归 emitter 的导出公开名回退合同。

use std::collections::BTreeMap;

use compiler_emitter::contracts::{Block, BlockRef, ExecutableFunction, Operand, Terminator, Value, ValueOrigin, ValueRef};
use compiler_target::cpython_module_name;
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

#[test]
fn cpython_module_name_normalizes_project() {
    // 工程名保留仓库身份，模块名是确定性的合法标识符：文件名、`PyInit_`、
    // 清单与宿主 `import` 必须共用同一个冻结值。
    assert_eq!(cpython_module_name("leetcode-multiply-strings").unwrap(), "leetcode_multiply_strings");
    assert_eq!(cpython_module_name("app").unwrap(), "app");
    assert_eq!(cpython_module_name("9lives").unwrap(), "_9lives");
    assert!(cpython_module_name("").is_err(), "空工程名无法归一化成合法模块名");
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

// wasm lane 的入口符号合同（`main` / `_start` 由宿主边界冻结、导出公开名冲突与
// 重复拒绝）已随纵链迁到语言侧 `language.valkyrie` 的 `pipeline/wasm/`，等价覆盖
// 见其 `wasm_lane_tests`。
