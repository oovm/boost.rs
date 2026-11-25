//! 导出命名合同：语言侧冻结源码名，各目标所有者按自己的命名合同投影。
//!
//! 机器码 lane（Node / CPython / 普通共享库）的投影与碰撞拒绝已随 lowering
//! 迁到语言侧 `language.valkyrie` 的机器码管道；本文件保留仍归 emitter 的
//! wasm lane 命名合同与 CPython 模块名归一化纯函数。

use std::collections::BTreeMap;

use compiler_emitter::{
    FragmentSubmission,
    contracts::{Block, BlockRef, ExecutableFunction, Operand, Terminator, Value, ValueOrigin, ValueRef},
};
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

/// 二进制包的导出公开名必须唯一，否则模块会写出重复导出名。
#[test]
fn wasm_binary_rejects_duplicate_export_public_names() {
    use compiler_target::HostProjectionBoundary;

    let mut plan = plan_with(&["entry_fn", "a", "b"], &[("a", "dup"), ("b", "dup")]);
    plan.testing_entry_operation(QualifiedName::new(vec![Identifier::new("entry_fn")]));
    let entry = plan.entry_operation().expect("入口已设置");
    plan.testing_export_names_mut().remove(&entry);
    let submission = FragmentSubmission::from_backend_plan(plan);
    let error = compiler_emitter::testing::lower_fragment_to_wasm_module(&submission, HostProjectionBoundary::WasmJsGlue)
        .expect_err("重复导出公开名必须拒绝");
    assert!(error.contains("重复"), "诊断应说明重复：{error}");
    assert!(error.contains("dup"), "诊断应指出冲突名：{error}");
}
