//! 运算符成员名的唯一所有者。
//!
//! Valkyrie 2020 不把运算符当作语义类别：`a + b` 就是 `a.infix +(b)` 的普通成员调用，
//! `!a` 就是 `a.prefix !()`，`a[i]` 就是 `a.suffix [ ](i)`。HIR/MIR/LIR 只保留普通成员名与
//! 普通 `Invoke`，不存在 operator/unary/binary 类别、`OperatorId` 或运算符身份表。
//!
//! 声明侧（`[infix] micro \`+\``）与调用侧（AST 运算符语法糖）必须得到同一个成员名，否则
//! 调用点会解析不到声明。因此「`{fixity} {lexeme}` 拼写」与「运算符词素 → 成员名」的映射
//! 只能有这一份所有者：HIR lowering 消费它，语义闭包规划消费它。
//!
//! 注意：本模块只给出**名字**，不给出语义。运算符没有独立身份，成员解析、重载匹配与
//! 歧义判定都在普通名字解析路径上完成。

use data_text_valkyrie::lexer::token_type::ValkyrieTokenType;

use crate::compiler::syntax::surface::ast::Attribute;

/// 序数下标 `receiver[i]` 的成员名。
pub const SUBSCRIPT_MEMBER: &str = "suffix [ ]";
/// 序数下标赋值 `receiver[i] = value` 的成员名。
pub const SUBSCRIPT_ASSIGN_MEMBER: &str = "suffix [ ]=";
/// 基数下标 `receiver::[i]` 的成员名。
pub const CARDINAL_SUBSCRIPT_MEMBER: &str = "suffix ⁅ ⁆";
/// 基数下标赋值 `receiver::[i] = value` 的成员名。
pub const CARDINAL_SUBSCRIPT_ASSIGN_MEMBER: &str = "suffix ⁅ ⁆=";
/// 解引用读取 `*receiver` 的成员名。
pub const DEREF_READ_MEMBER: &str = "deref_read";

/// for-in 脱糖 `coll.into_iterator()` 的成员名（`IntoIterator` 协议）。
pub const FOR_IN_INTO_ITERATOR_MEMBER: &str = "into_iterator";
/// for-in 脱糖 `it.has_next()` 的成员名（`Iterator` 协议）。
pub const FOR_IN_HAS_NEXT_MEMBER: &str = "has_next";
/// for-in 脱糖 `it.next()` 的成员名（`Iterator` 协议）。
pub const FOR_IN_NEXT_MEMBER: &str = "next";
/// for-in 脱糖取迭代项的 `Option::unwrap()` 成员名。
pub const FOR_IN_UNWRAP_MEMBER: &str = "unwrap";

/// for-in 迭代协议的选择名集合。
///
/// for-in 在 HIR 下降期脱糖为迭代协议成员调用（`lower_for_in_desugar`），这些调用
/// 不出现在源码里；语义闭包必须按同一份成员名把协议 witness 纳入需求，否则脱糖
/// 后的调用点没有声明可解析。两个消费方（闭包规划、HIR 脱糖）只准引用本常量组。
pub const FOR_IN_PROTOCOL_SELECTORS: [&str; 4] =
    [FOR_IN_INTO_ITERATOR_MEMBER, FOR_IN_HAS_NEXT_MEMBER, FOR_IN_NEXT_MEMBER, FOR_IN_UNWRAP_MEMBER];

/// 字符串插值脱糖 `<expr>.display()` 的成员名（§4.10 E）。
///
/// 插值段在 HIR 下降期改写为普通成员调用，调用名不出现在源码里；语义闭包必须按同一
/// 成员名把 formatter witness 纳入需求，否则脱糖后的调用点没有声明可解析。闭包规划与
/// HIR 脱糖共用本常量，不各写一份拼写。
pub const INTERPOLATION_DISPLAY_MEMBER: &str = "display";

/// for-in 迭代协议 trait 的规范命名空间（语言规范 `std.iterator`）。
///
/// 协议义务的身份是 trait 声明本身，不是接收者上的同名成员：for-in 必须以
/// `IntoIterator` 的声明身份结算，禁止用方法名探测替代 obligation。
pub const FOR_IN_PROTOCOL_NAMESPACE: [&str; 2] = ["std", "iterator"];
/// `IntoIterator` 协议 trait 名。
pub const FOR_IN_INTO_ITERATOR_TRAIT: &str = "IntoIterator";

/// `{fixity} {lexeme}`：运算符成员名的唯一拼写。
///
/// fixity 只允许 `prefix` / `infix` / `suffix` 三种词素；`prefix \`-\`` 与 `infix \`-\``
/// 共享词素，只有组合名能让两者在普通成员解析与调用点匹配中区分开。
pub fn fixed_member_name(fixity: &str, lexeme: &str) -> String {
    format!("{fixity} {lexeme}")
}

/// 中缀运算符调用的成员名（`a + b` → `a.infix +(b)`）。
pub fn infix_member_name(operator: &ValkyrieTokenType) -> String {
    let lexeme = match operator {
        ValkyrieTokenType::Plus => "+",
        ValkyrieTokenType::PlusPlus => "++",
        ValkyrieTokenType::Minus => "-",
        ValkyrieTokenType::Star => "*",
        ValkyrieTokenType::Slash => "/",
        ValkyrieTokenType::Percent => "%",
        ValkyrieTokenType::EqEq => "==",
        ValkyrieTokenType::NotEq => "!=",
        ValkyrieTokenType::LessThan => "<",
        ValkyrieTokenType::LessEq => "<=",
        ValkyrieTokenType::GreaterThan => ">",
        ValkyrieTokenType::GreaterEq => ">=",
        ValkyrieTokenType::LeftShift => "<<",
        ValkyrieTokenType::RightShift => ">>",
        ValkyrieTokenType::Ampersand => "&",
        ValkyrieTokenType::Pipe => "|",
        ValkyrieTokenType::Caret => "^",
        ValkyrieTokenType::DotDot => "..",
        ValkyrieTokenType::DotDotEq => "..=",
        // `AndAnd` / `OrOr` / `Eq` 降低为控制流或赋值，不产生成员调用，调用方不会走到这里。
        _ => "+",
    };
    fixed_member_name("infix", lexeme)
}

/// 前缀运算符调用的成员名（`!a` → `a.prefix !()`）。
pub fn prefix_member_name(operator: &ValkyrieTokenType) -> String {
    let lexeme = match operator {
        ValkyrieTokenType::Bang => "!",
        _ => "-",
    };
    fixed_member_name("prefix", lexeme)
}

/// 运算符声明的成员名：`[infix] micro \`+\`` → `infix +`；非运算符声明保持原名。
///
/// fixity 判定与拼写都只在这里：声明侧（本函数）与调用侧（[`infix_member_name`] /
/// [`prefix_member_name`]）必须给出同一个名字，闭包、HIR 与后端都消费这个名字。
pub fn declared_member_name(declared: &str, annotations: &[Attribute]) -> String {
    let fixity = if has_modifier(annotations, "prefix") {
        "prefix"
    }
    else if has_modifier(annotations, "infix") {
        "infix"
    }
    else if has_modifier(annotations, "postfix") || has_modifier(annotations, "suffix") {
        "suffix"
    }
    else {
        return declared.to_string();
    };
    fixed_member_name(fixity, declared)
}

/// 调用点/声明侧的普通成员名是否为运算符成员（`{fixity} {lexeme}` 或 `deref_read`）。
pub fn is_operator_member_name(name: &str) -> bool {
    name.starts_with("prefix ") || name.starts_with("infix ") || name.starts_with("suffix ") || name == DEREF_READ_MEMBER
}

/// 声明是否带有指定修饰注解。
fn has_modifier(annotations: &[Attribute], name: &str) -> bool {
    annotations.iter().any(|attribute| attribute.name.name == name)
}

#[cfg(test)]
mod tests {
    use compiler_target::{RewriteTheory, TheoryBundle};
    use compiler_types::SemanticType;

    use crate::compiler::hir::lowering::{CompilerSourceFile, CompilerSourceGroup};
    use crate::compiler::hir::ValkyrieCompiler;

    /// 语言测试夹具的标准库源：运算符作为结构体内的普通成员声明。
    const STD_OPERATORS: &str =
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/std_operators.v"));

    fn compile_scalar_operator_probe() -> crate::compiler::valkyrie_mir::CompiledValkyrieProgram {
        let source = r#"
namespace app;

[export(name: "add")]
micro add(a: i32, b: i32) -> i32 { return a + b }
[export(name: "less")]
micro less(a: i32, b: i32) -> bool { return a < b }
[export(name: "less_or_equal")]
micro less_or_equal(a: i32, b: i32) -> bool { return a <= b }
[export(name: "wide_ge")]
micro wide_ge(a: u32, b: u32) -> bool { return a >= b }
[export(name: "negate")]
micro negate(a: i32) -> i32 { return -a }
"#;
        let group = CompilerSourceGroup::from_source_files(
            "app",
            "app",
            vec![
                CompilerSourceFile { path: "std_operators.v".into(), source: STD_OPERATORS.into() },
                CompilerSourceFile { path: "main.v".into(), source: source.into() },
            ],
            Vec::new(),
        )
        .as_entry();
        ValkyrieCompiler::default()
            .compile_source_groups_to_program(&[group])
            .unwrap_or_else(|error| panic!("`a + b` / `a < b` / `-a` 必须解析到运算符成员声明: {error}"))
    }

    /// 前端闭环：`[infix] micro \`+\`` 是普通成员声明；调用点解析到该声明，
    /// 目标绑定注解在目标计划里成为 operator 导入链接（不是 Unfrozen，也不是宿主导入）。
    #[test]
    fn scalar_operator_member_declarations_resolve_and_link_target_bindings() {
        let program = compile_scalar_operator_probe();
        let canonical = program.mir_program();
        let fragment = canonical.linked.fragments.keys().next().cloned().expect("程序必须至少声明一个片段");
        let roots: Vec<_> = canonical.mir.functions.keys().copied().collect();
        let build_plan = |family: compiler_target::TargetBackendFamily| {
            crate::compiler::emitter::pipeline::target_plan::from_compiled_program(
                &program,
                &fragment,
                TheoryBundle { shared: RewriteTheory::default(), fragment: RewriteTheory::default() },
                &roots,
                compiler_emitter::sum_tag_width(family),
            )
            .unwrap_or_else(|error| panic!("运算符实例必须能冻结进目标计划({family:?}): {error}"))
        };
        let assert_plan = |plan: &compiler_emitter::TargetExecutionPlan| {
            let by_segment = |segment: &str| {
                plan.imports()
                    .values()
                    .find(|import| import.links.iter().any(|link| link.locator_segments().iter().any(|part| part == segment)))
                    .unwrap_or_else(|| panic!("目标计划缺少 `{segment}` operator 导入"))
            };

            let add = by_segment("integer.add");
            // 四个 target 的绑定必须按声明顺序全部保留：语言侧只记录「声明了哪些目标绑定」，
            // 选哪条由各 target 自己的裁决函数决定，前端不得 first-wins 丢弃其余三条。
            let add_bindings = add
                .links
                .iter()
                .map(|link| {
                    assert_eq!(link.boundary.as_str(), "host");
                    (link.platform_tag.as_ref().map(|tag| tag.as_str().to_string()), link.locator_segments().join("."))
                })
                .collect::<Vec<_>>();
            assert_eq!(
                add_bindings,
                vec![
                    (Some("jvm-operator".to_string()), "integer.add".to_string()),
                    (Some("clr-operator".to_string()), "i32.add".to_string()),
                    (Some("wasm-operator".to_string()), "i32.add".to_string()),
                    (Some("ovm-operator".to_string()), "i32.add".to_string()),
                ]
            );
            assert_eq!(
                add.parameter_types,
                vec![SemanticType::Integer32 { signed: true }, SemanticType::Integer32 { signed: true }]
            );
            assert_eq!(add.return_type, SemanticType::Integer32 { signed: true });

            let less = by_segment("integer.compare_less_than");
            assert_eq!(less.parameter_types, vec![SemanticType::Integer32 { signed: true }, SemanticType::Integer32 { signed: true }]);
            assert_eq!(less.return_type, SemanticType::Boolean);

            // 有序比较的另一半（`<=`）也必须从同一份声明解析成普通成员调用，
            // 而不是靠后端名字猜测；这里只核对语言侧落进计划的签名。
            let less_or_equal = by_segment("integer.compare_less_or_equal");
            assert_eq!(
                less_or_equal.parameter_types,
                vec![SemanticType::Integer32 { signed: true }, SemanticType::Integer32 { signed: true }]
            );
            assert_eq!(less_or_equal.return_type, SemanticType::Boolean);

            // 无符号载体不是新语义类别：`u32` 的有序比较仍解析成普通成员调用，
            // 只是目标绑定助记符换成无符号裁决。
            let unsigned = by_segment("integer.compare_greater_or_equal_unsigned");
            assert_eq!(
                unsigned.parameter_types,
                vec![SemanticType::Integer32 { signed: false }, SemanticType::Integer32 { signed: false }]
            );
            assert_eq!(unsigned.return_type, SemanticType::Boolean);

            let negate = by_segment("integer.negate");
            assert_eq!(negate.parameter_types, vec![SemanticType::Integer32 { signed: true }]);
            assert_eq!(negate.return_type, SemanticType::Integer32 { signed: true });
        };

        // 同一份 `std_operators.v` 分别以四个 target 走「解析 → 计划」：每个计划都必须
        // 保留全部四条候选链接，target 差异只体现在 sum tag width，不改变声明保留。
        assert_plan(&build_plan(compiler_target::TargetBackendFamily::Jvm));
        assert_plan(&build_plan(compiler_target::TargetBackendFamily::Clr));
        assert_plan(&build_plan(compiler_target::TargetBackendFamily::Wasm));
        assert_plan(&build_plan(compiler_target::TargetBackendFamily::Ovm));
    }
}
