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

/// e2e 覆盖的 target：legion 标签与运算符成员上的绑定注解名。
///
/// 能力声明落在「运算符成员是否有该 target 的绑定注解」上：某 target 没有该
/// 助记符就声明 `None`，用例可运行的 target 集合由声明推导（`Case::supports`）。
/// 这里不写「按 target 名字跳过」的名单——配方补齐后用例会自动进入该 target。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    Jvm,
    Clr,
    Wasm,
    Ovm,
}

/// 全部 target，推导与遍历顺序的唯一来源。
const TARGETS: &[Target] = &[Target::Jvm, Target::Clr, Target::Wasm, Target::Ovm];

impl Target {
    /// legion 构建/清单里的 target 标签。
    fn label(self) -> &'static str {
        match self {
            Self::Jvm => "jvm",
            Self::Clr => "clr",
            Self::Wasm => "wasm-node",
            Self::Ovm => "ovm",
        }
    }

    /// 运算符成员上的目标绑定注解名。
    fn binding_attribute(self) -> &'static str {
        match self {
            Self::Jvm => "jvm_operator",
            Self::Clr => "clr_operator",
            Self::Wasm => "wasm_operator",
            Self::Ovm => "ovm_operator",
        }
    }

    /// 该 target 在这个运算符上的冻结助记符。
    ///
    /// `None` 是真实配方缺口：不得用别的 target 的助记符顶替，也不得靠名字特判绕开。
    fn mnemonic(self, operator: &Operator) -> Option<&'static str> {
        match self {
            Self::Jvm => operator.jvm,
            Self::Clr => operator.clr,
            Self::Wasm => operator.wasm,
            Self::Ovm => operator.ovm,
        }
    }
}

/// 一个运算符成员声明在四个 target 上的绑定。
struct Operator {
    /// `infix` / `prefix`。
    form: &'static str,
    /// 反引号内的运算符词法形式。
    symbol: &'static str,
    /// 二元运算符声明 `other` 形参。
    binary: bool,
    /// 比较运算符声明返回 `bool`；算术返回载体自身。
    comparison: bool,
    jvm: Option<&'static str>,
    clr: Option<&'static str>,
    wasm: Option<&'static str>,
    ovm: Option<&'static str>,
}

const fn infix(
    symbol: &'static str,
    comparison: bool,
    jvm: Option<&'static str>,
    clr: Option<&'static str>,
    wasm: Option<&'static str>,
    ovm: Option<&'static str>,
) -> Operator {
    Operator { form: "infix", symbol, binary: true, comparison, jvm, clr, wasm, ovm }
}

const fn prefix(
    symbol: &'static str,
    jvm: Option<&'static str>,
    clr: Option<&'static str>,
    wasm: Option<&'static str>,
    ovm: Option<&'static str>,
) -> Operator {
    Operator { form: "prefix", symbol, binary: false, comparison: false, jvm, clr, wasm, ovm }
}

/// 一个 `[primitive]` 结构及其运算符成员，用于按 target 渲染 `core_primitive.v`。
struct Primitive {
    /// 规范限定名（`[primitive("...")]`）。
    canonical: &'static str,
    /// 源里的结构名，也是运算符 `self` 的载体类型。
    name: &'static str,
    operators: &'static [Operator],
}

impl Primitive {
    /// 渲染该 target 的 `[primitive]` 结构。
    ///
    /// 调用方只在 `Case::supports` 成立后渲染，因此缺绑定是内部错误：渲染阶段
    /// 绝不补造注解或省略成员。
    fn render(&self, target: Target) -> String {
        let mut text = format!("[primitive(\"{}\")]\nstructure {} {{\n", self.canonical, self.name);
        for operator in self.operators {
            let mnemonic = target.mnemonic(operator).unwrap_or_else(|| {
                panic!("`{}` 的运算符 `{}` 在 {target:?} 上没有绑定：必须先经 `Case::supports` 过滤", self.name, operator.symbol)
            });
            let signature = if operator.binary { format!("self: {}, other: {}", self.name, self.name) } else { format!("self: {}", self.name) };
            let result = if operator.comparison { "bool" } else { self.name };
            text.push_str(&format!(
                "    [{}]\n    [{}(\"{}\")]\n    micro `{}`({}) -> {};\n\n",
                operator.form,
                target.binding_attribute(),
                mnemonic,
                operator.symbol,
                signature,
                result,
            ));
        }
        text.push_str("}\n\n");
        text
    }
}

/// 有符号 `i32` 的完整算术表面：`+ - * / %`、前缀 `-` 与六个比较，四 target 齐全。
const I32_OPERATORS: &[Operator] = &[
    infix("+", false, Some("integer.add"), Some("i32.add"), Some("i32.add"), Some("i32.add")),
    infix("-", false, Some("integer.subtract"), Some("i32.sub"), Some("i32.sub"), Some("i32.sub")),
    infix("*", false, Some("integer.multiply"), Some("i32.mul"), Some("i32.mul"), Some("i32.mul")),
    infix("/", false, Some("integer.divide"), Some("i32.div"), Some("i32.div_s"), Some("i32.div")),
    infix("%", false, Some("integer.remainder"), Some("i32.rem"), Some("i32.rem_s"), Some("i32.rem")),
    prefix("-", Some("integer.negate"), Some("i32.neg"), Some("i32.neg"), Some("i32.neg")),
    infix("==", true, Some("integer.compare_equal"), Some("i32.eq"), Some("i32.eq"), Some("i32.eq")),
    infix("!=", true, Some("integer.compare_not_equal"), Some("i32.ne"), Some("i32.ne"), Some("i32.ne")),
    infix("<", true, Some("integer.compare_less_than"), Some("i32.lt"), Some("i32.lt_s"), Some("i32.lt")),
    infix("<=", true, Some("integer.compare_less_or_equal"), Some("i32.le"), Some("i32.le_s"), Some("i32.le")),
    infix(">", true, Some("integer.compare_greater_than"), Some("i32.gt"), Some("i32.gt_s"), Some("i32.gt")),
    infix(">=", true, Some("integer.compare_greater_or_equal"), Some("i32.ge"), Some("i32.ge_s"), Some("i32.ge")),
];

/// 有符号用例只用到 `u32` 的前缀 `-` 与无符号 `>`。
const U32_NEGATE_AND_COMPARE: &[Operator] = &[
    prefix("-", Some("integer.negate"), Some("i32.neg"), Some("i32.neg"), Some("i32.neg")),
    infix(">", true, Some("integer.compare_greater_than_unsigned"), Some("i32.gt.un"), Some("i32.gt_u"), Some("i32.gt_u")),
];

/// 无符号 `u32` 除/余：OVM 没有无符号除余头码，因此不声明 ovm 绑定。
const U32_UNSIGNED_DIVISION: &[Operator] = &[
    prefix("-", Some("integer.negate"), Some("i32.neg"), Some("i32.neg"), Some("i32.neg")),
    infix("/", false, Some("integer.divide_unsigned"), Some("i32.div.un"), Some("i32.div_u"), None),
    infix("%", false, Some("integer.remainder_unsigned"), Some("i32.rem.un"), Some("i32.rem_u"), None),
    infix(">", true, Some("integer.compare_greater_than_unsigned"), Some("i32.gt.un"), Some("i32.gt_u"), Some("i32.gt_u")),
    infix("<", true, Some("integer.compare_less_than_unsigned"), Some("i32.lt.un"), Some("i32.lt_u"), Some("i32.lt_u")),
    infix("<=", true, Some("integer.compare_less_or_equal_unsigned"), Some("i32.le.un"), Some("i32.le_u"), Some("i32.le_u")),
    infix(">=", true, Some("integer.compare_greater_or_equal_unsigned"), Some("i32.ge.un"), Some("i32.ge_u"), Some("i32.ge_u")),
    // `u32 >> n` 必须是逻辑右移：`i32.shr.un`/`i32.shr_u`/`iushr`，不得借算术右移。
    infix(">>", false, Some("integer.shift_right_unsigned"), Some("i32.shr.un"), Some("i32.shr_u"), Some("i32.shr_u")),
];

/// `f64` 算术与比较：OVM 没有 f64 头码，CLR/Wasm 没有浮点余数，故不声明 `%`。
const F64_OPERATORS: &[Operator] = &[
    infix("+", false, Some("double.add"), Some("f64.add"), Some("f64.add"), None),
    infix("-", false, Some("double.subtract"), Some("f64.sub"), Some("f64.sub"), None),
    infix("*", false, Some("double.multiply"), Some("f64.mul"), Some("f64.mul"), None),
    infix("/", false, Some("double.divide"), Some("f64.div"), Some("f64.div"), None),
    prefix("-", Some("double.negate"), Some("f64.neg"), Some("f64.neg"), None),
    infix("==", true, Some("double.compare_equal"), Some("f64.eq"), Some("f64.eq"), None),
    infix("!=", true, Some("double.compare_not_equal"), Some("f64.ne"), Some("f64.ne"), None),
    infix("<", true, Some("double.compare_less_than"), Some("f64.lt"), Some("f64.lt"), None),
    infix("<=", true, Some("double.compare_less_or_equal"), Some("f64.le"), Some("f64.le"), None),
    infix(">", true, Some("double.compare_greater_than"), Some("f64.gt"), Some("f64.gt"), None),
    infix(">=", true, Some("double.compare_greater_or_equal"), Some("f64.ge"), Some("f64.ge"), None),
];

/// 有符号 `i64` 算术与比较：jvm / clr / wasm 齐全，OVM 没有 i64 头码。
const I64_OPERATORS: &[Operator] = &[
    infix("+", false, Some("long.add"), Some("i64.add"), Some("i64.add"), None),
    infix("-", false, Some("long.subtract"), Some("i64.sub"), Some("i64.sub"), None),
    infix("*", false, Some("long.multiply"), Some("i64.mul"), Some("i64.mul"), None),
    infix("/", false, Some("long.divide"), Some("i64.div"), Some("i64.div_s"), None),
    infix("%", false, Some("long.remainder"), Some("i64.rem"), Some("i64.rem_s"), None),
    prefix("-", Some("long.negate"), Some("i64.neg"), Some("i64.neg"), None),
    infix("==", true, Some("long.compare_equal"), Some("i64.eq"), Some("i64.eq"), None),
    infix("!=", true, Some("long.compare_not_equal"), Some("i64.ne"), Some("i64.ne"), None),
    infix("<", true, Some("long.compare_less_than"), Some("i64.lt"), Some("i64.lt_s"), None),
    infix("<=", true, Some("long.compare_less_or_equal"), Some("i64.le"), Some("i64.le_s"), None),
    infix(">", true, Some("long.compare_greater_than"), Some("i64.gt"), Some("i64.gt_s"), None),
    infix(">=", true, Some("long.compare_greater_or_equal"), Some("i64.ge"), Some("i64.ge_s"), None),
];

/// 无符号 `u64`：除/余与六个有序比较都必须用无符号助记符；OVM 没有 i64 头码。
const U64_OPERATORS: &[Operator] = &[
    prefix("-", Some("long.negate"), Some("i64.neg"), Some("i64.neg"), None),
    infix("/", false, Some("long.divide_unsigned"), Some("i64.div.un"), Some("i64.div_u"), None),
    infix("%", false, Some("long.remainder_unsigned"), Some("i64.rem.un"), Some("i64.rem_u"), None),
    infix("==", true, Some("long.compare_equal"), Some("i64.eq"), Some("i64.eq"), None),
    infix("!=", true, Some("long.compare_not_equal"), Some("i64.ne"), Some("i64.ne"), None),
    infix("<", true, Some("long.compare_less_than_unsigned"), Some("i64.lt.un"), Some("i64.lt_u"), None),
    infix("<=", true, Some("long.compare_less_or_equal_unsigned"), Some("i64.le.un"), Some("i64.le_u"), None),
    infix(">", true, Some("long.compare_greater_than_unsigned"), Some("i64.gt.un"), Some("i64.gt_u"), None),
    infix(">=", true, Some("long.compare_greater_or_equal_unsigned"), Some("i64.ge.un"), Some("i64.ge_u"), None),
    // `u64 >> n` 必须是逻辑右移：`lushr`/`i64.shr.un`/`i64.shr_u`，不得借算术右移。
    infix(">>", false, Some("long.shift_right_unsigned"), Some("i64.shr.un"), Some("i64.shr_u"), None),
];

/// `i32` 位运算与移位：`& | ^ << >>`，四个 target 头码齐全。
const I32_BITWISE: &[Operator] = &[
    infix("&", false, Some("integer.and"), Some("i32.and"), Some("i32.and"), Some("i32.and")),
    infix("|", false, Some("integer.or"), Some("i32.or"), Some("i32.or"), Some("i32.or")),
    infix("^", false, Some("integer.xor"), Some("i32.xor"), Some("i32.xor"), Some("i32.xor")),
    infix("<<", false, Some("integer.shift_left"), Some("i32.shl"), Some("i32.shl"), Some("i32.shl")),
    infix(">>", false, Some("integer.shift_right"), Some("i32.shr"), Some("i32.shr_s"), Some("i32.shr_s")),
    prefix("-", Some("integer.negate"), Some("i32.neg"), Some("i32.neg"), Some("i32.neg")),
    infix("==", true, Some("integer.compare_equal"), Some("i32.eq"), Some("i32.eq"), Some("i32.eq")),
];

/// 宽载体 `i64` 位运算与移位：jvm / clr / wasm 齐全，OVM 没有 i64 头码。
const I64_BITWISE: &[Operator] = &[
    infix("&", false, Some("long.and"), Some("i64.and"), Some("i64.and"), None),
    infix("|", false, Some("long.or"), Some("i64.or"), Some("i64.or"), None),
    infix("^", false, Some("long.xor"), Some("i64.xor"), Some("i64.xor"), None),
    infix("<<", false, Some("long.shift_left"), Some("i64.shl"), Some("i64.shl"), None),
    infix(">>", false, Some("long.shift_right"), Some("i64.shr"), Some("i64.shr_s"), None),
    prefix("-", Some("long.negate"), Some("i64.neg"), Some("i64.neg"), None),
    infix("==", true, Some("long.compare_equal"), Some("i64.eq"), Some("i64.eq"), None),
    infix("<", true, Some("long.compare_less_than"), Some("i64.lt"), Some("i64.lt_s"), None),
];

/// 只作为返回/比较结果载体出现、不参与任何运算的 primitive。
const NO_OPERATORS: &[Operator] = &[];

const I32_PRIMITIVE: Primitive = Primitive { canonical: "std::primitive::i32", name: "i32", operators: I32_OPERATORS };
const U32_ARITHMETIC_PRIMITIVE: Primitive = Primitive { canonical: "std::primitive::u32", name: "u32", operators: U32_NEGATE_AND_COMPARE };
const U32_DIVISION_PRIMITIVE: Primitive = Primitive { canonical: "std::primitive::u32", name: "u32", operators: U32_UNSIGNED_DIVISION };
const F64_PRIMITIVE: Primitive = Primitive { canonical: "std::primitive::f64", name: "f64", operators: F64_OPERATORS };
const I64_PRIMITIVE: Primitive = Primitive { canonical: "std::primitive::i64", name: "i64", operators: I64_OPERATORS };
const BITWISE_I32_PRIMITIVE: Primitive = Primitive { canonical: "std::primitive::i32", name: "i32", operators: I32_BITWISE };
const BITWISE_I64_PRIMITIVE: Primitive = Primitive { canonical: "std::primitive::i64", name: "i64", operators: I64_BITWISE };
const U64_PRIMITIVE: Primitive = Primitive { canonical: "std::primitive::u64", name: "u64", operators: U64_OPERATORS };
const CARRIER_I32: Primitive = Primitive { canonical: "std::primitive::i32", name: "i32", operators: NO_OPERATORS };
const CARRIER_BOOL: Primitive = Primitive { canonical: "std::primitive::bool", name: "bool", operators: NO_OPERATORS };

/// 含运算符的可观测算术入口：`mix(10, 4) + wide_gt(1)` 返回 13。
///
/// `(10 + 4) * 3 - 10 / 2 = 37`，`37 % 7 = 2`，前缀 `-` 得 -2，此时
/// `x < negated`、`x == negated`、`x <= negated` 均不成立、`x != negated` 成立，
/// `x > negated` 与 `x >= negated` 成立，`mix` 返回 `negated + r + 6 = 6`。
/// 三个有序比较各自承担不同的错误分支（`<=` 误判返回 2、`>` 误判返回 0、
/// `>=` 误判返回 3），因此结果 6 能同时判定三者。
///
/// `wide_gt(1)` 把 `u32` 前缀取负得到 `0xFFFF_FFFF`：无符号 `>` 判 true 返回 7，
/// 若后端误用有符号比较则 -1 > 1 为 false 返回 2。合计期望 13，可同时区分
/// 有符号/无符号裁决错误。
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
        if x <= negated {
            return 2
        }
        if x > negated {
            if x >= negated {
                return negated + r + 6
            }
            return 3
        }
    }
    return 0
}

micro wide_gt(a: u32) -> i32 {
    let big = -a
    if big > a {
        return 7
    }
    return 2
}

[main]
micro main() -> i32 {
    return app::mix(10, 4) + app::wide_gt(1)
}
"#;

/// `f64` 用例入口：`classify(1.5)` 走 `+ - * /`、前缀 `-` 与六个比较。
///
/// `x = 1.5 * 2.0 + 0.5 = 3.5`，`y = x / 2.0 = 1.75`，`neg = -y = -1.75`，
/// `y - neg = 3.5` 在二进制里精确。六个比较按嵌套顺序各带一个独立错误分支
/// （2/3/4/5/6），全部成立才返回 17，因此 17 能同时判定六个比较助记符。
const FLOAT_SOURCE: &str = r#"namespace app;

micro classify(a: f64) -> i32 {
    let x = a * 2.0 + 0.5
    let y = x / 2.0
    let neg = -y
    if y > neg {
        if y - neg == 3.5 {
            if y != neg {
                if y >= neg {
                    if neg < y {
                        if neg <= y {
                            return 17
                        }
                        return 6
                    }
                    return 5
                }
                return 4
            }
            return 3
        }
        return 2
    }
    return 1
}

[main]
micro main() -> i32 {
    return app::classify(1.5)
}
"#;

/// 无符号 `u32` 用例入口：`probe(1)` 在 `0xFFFF_FFFF` 上做 `/ 2`、`% 7`、四个有序比较与 `>> 4`。
///
/// 无符号：`/ 2 = 0x7FFF_FFFF`、`% 7 = 3`、`>> 4 = 0x0FFF_FFFF`，商上 `> >=` 成立、
/// 余数上 `< <=` 成立、移位后 `< big` 成立。若 `/` 误用有符号除法，商为 0、`0 > 1`
/// 不成立而返回 1；若 `%` 误用有符号取余，余数为 `0xFFFF_FFFF`，`< 7`、`<= 7` 都不成立；
/// 若 `>>` 误用算术右移，移位结果仍为 `0xFFFF_FFFF`、`< big` 不成立而返回 7。
const UNSIGNED_DIVISION_SOURCE: &str = r#"namespace app;

micro probe(a: u32) -> i32 {
    let big = -a
    let quotient = big / 2
    let remainder = big % 7
    let shifted = big >> 4
    if quotient > a {
        if quotient >= a {
            if remainder < 7 {
                if remainder <= 7 {
                    if shifted < big {
                        return 5
                    }
                    return 7
                }
                return 6
            }
            return 4
        }
        return 3
    }
    return 1
}

[main]
micro main() -> i32 {
    return app::probe(1)
}
"#;

/// `i64` 用例入口：`wide(5)` 在宽载体上走 `+ - * / %`、前缀 `-` 与六个比较。
///
/// `x = 5 * 4 - 4 + 1 = 17`，`y = 17 / 5 = 3`，`r = 17 % 5 = 2`，`neg = -2`。
/// 六个比较按嵌套顺序各带独立错误分支（2/3/4/5/6），全部成立返回 23，
/// 因此 23 能同时判定宽载体的六个比较助记符与除法/取余/取负。
const INT64_SOURCE: &str = r#"namespace app;

micro wide(a: i64) -> i32 {
    let x = a * 4 - 4 + 1
    let y = x / 5
    let r = x % 5
    let neg = -r
    if x == 17 {
        if x > y {
            if y >= r {
                if y != neg {
                    if x <= 17 {
                        if neg < r {
                            return 23
                        }
                        return 6
                    }
                    return 5
                }
                return 4
            }
            return 3
        }
        return 2
    }
    return 1
}

[main]
micro main() -> i32 {
    return app::wide(5)
}
"#;

/// 无符号 `u64` 用例入口：`0xFFFF_FFFF_FFFF_FFFF` 上的除/余、`>> 32` 与六个无符号比较。
///
/// 无符号：`/ 2 = 0x7FFF_FFFF_FFFF_FFFF`、`% 7 = 1`、`>> 32 = 0xFFFF_FFFF`。若误用有符号
/// 裁决，`q > a` 不成立返回 1；若 `%` 误用有符号取余，余数为全 1、`r < 7` 不成立返回 2；
/// 若 `>>` 误用算术右移，结果仍为全 1、`logical < big` 不成立返回 7；其余比较分支
/// 分别返回 3/4/5/6。全部成立返回 31。
const UINT64_SOURCE: &str = r#"namespace app;

micro wide_unsigned(a: u64) -> i32 {
    let big = -a
    let q = big / 2
    let r = big % 7
    let logical = big >> 32
    if q > a {
        if q >= a {
            if r < 7 {
                if r <= 7 {
                    if logical < big {
                        if a != big {
                            if a == 1 {
                                return 31
                            }
                            return 6
                        }
                        return 3
                    }
                    return 7
                }
                return 4
            }
            return 2
        }
        return 5
    }
    return 1
}

[main]
micro main() -> i32 {
    return app::wide_unsigned(1)
}
"#;

/// `i32` 位运算用例入口：`bits(13)` 走 `& | ^ << >>`、前缀 `-` 与 `==`。
///
/// `13 & 7 = 5`，`5 | 8 = 13`，`13 ^ 15 = 2`，`2 << 3 = 16`，`-16 >> 2 = -4`
/// （有符号右移必须算术填充）。每步带独立错误分支；全部成立返回 29。若 `>>` 误用
/// 逻辑右移，`-16 >> 2 = 0x3FFF_FFFC`，`r == -4` 不成立而返回 5。
const BITWISE_SOURCE: &str = r#"namespace app;

micro bits(a: i32) -> i32 {
    let x = a & 7
    let y = x | 8
    let z = y ^ 15
    let shifted = z << 3
    let neg = -shifted
    let r = neg >> 2
    if x == 5 {
        if y == 13 {
            if z == 2 {
                if shifted == 16 {
                    if r == -4 {
                        return 29
                    }
                    return 5
                }
                return 4
            }
            return 3
        }
        return 2
    }
    return 1
}

[main]
micro main() -> i32 {
    return app::bits(13)
}
"#;

/// 宽载体位运算用例入口：`wide_bits(1)` 走 i64 的 `<< >> & | ^`、前缀 `-` 与比较。
///
/// `x = 1 << 33 = 0x2_0000_0000`，`y = x | 15`，`z = y ^ 15 = x`，`z & 255 = 0`，
/// `neg = -x`，`half = neg >> 32 = -2`（有符号右移必须算术填充）。全部成立返回 37。
/// 若 `>>` 误用逻辑右移，`neg >> 32` 得 `0xFFFF_FFFE`（正数），`half < 0` 不成立而
/// 返回 4；若移位量被截成 32 位以下或位运算助记符错位，后续分支各自给出不同值。
const WIDE_BITWISE_SOURCE: &str = r#"namespace app;

micro wide_bits(a: i64) -> i32 {
    let x = a << 33
    let y = x | 15
    let z = y ^ 15
    let low = z & 255
    let neg = -x
    let half = neg >> 32
    if z == x {
        if low == 0 {
            if half < 0 {
                if half == -2 {
                    return 37
                }
                return 5
            }
            return 4
        }
        return 3
    }
    return 2
}

[main]
micro main() -> i32 {
    return app::wide_bits(1)
}
"#;

/// 一个数值算子 e2e 用例：夹具声明 + 可观测入口。
struct Case {
    /// 临时目录与产物目录的去重标签。
    label: &'static str,
    primitives: &'static [Primitive],
    source: &'static str,
    /// 入口返回值的期望：jvm 打印、clr/wasm 退出码、ovm 进程内 `Value::I32`。
    expected: i32,
}

impl Case {
    /// 只有该 target 上每个被声明的运算符都有冻结助记符时才运行。
    ///
    /// 能力来自夹具声明本身：缺注解即缺配方，用例在该 target 上不成立；补齐配方后
    /// 无需改测试就会自动纳入。
    fn supports(&self, target: Target) -> bool {
        self.primitives.iter().all(|primitive| primitive.operators.iter().all(|operator| target.mnemonic(operator).is_some()))
    }

    /// 该 target 的 `core_primitive.v` 全文。
    ///
    /// 所有 primitive 声明都归 `std.primitive` 命名空间：`[primitive]` 身份
    /// （`std::primitive::i32` 等）在携带者可见面内解析，缺命名空间会落空。
    fn primitive_source(&self, target: Target) -> String {
        let mut text = String::from("namespace std.primitive;\n\n");
        text.push_str(&self.primitives.iter().map(|primitive| primitive.render(target)).collect::<String>());
        text
    }
}

const ARITHMETIC_PRIMITIVES: &[Primitive] = &[I32_PRIMITIVE, U32_ARITHMETIC_PRIMITIVE, CARRIER_BOOL];
const FLOAT_PRIMITIVES: &[Primitive] = &[CARRIER_I32, F64_PRIMITIVE, CARRIER_BOOL];
const UNSIGNED_DIVISION_PRIMITIVES: &[Primitive] = &[CARRIER_I32, U32_DIVISION_PRIMITIVE, CARRIER_BOOL];
const INT64_PRIMITIVES: &[Primitive] = &[CARRIER_I32, I64_PRIMITIVE, CARRIER_BOOL];
const UINT64_PRIMITIVES: &[Primitive] = &[CARRIER_I32, U64_PRIMITIVE, CARRIER_BOOL];
const BITWISE_PRIMITIVES: &[Primitive] = &[BITWISE_I32_PRIMITIVE, CARRIER_BOOL];
const WIDE_BITWISE_PRIMITIVES: &[Primitive] = &[CARRIER_I32, BITWISE_I64_PRIMITIVE, CARRIER_BOOL];

/// 全部数值算子用例。`expected` 是入口返回值，不是产物存在性。
const NUMERIC_CASES: &[Case] = &[
    Case { label: "arithmetic", primitives: ARITHMETIC_PRIMITIVES, source: ARITHMETIC_SOURCE, expected: 13 },
    Case { label: "float64", primitives: FLOAT_PRIMITIVES, source: FLOAT_SOURCE, expected: 17 },
    Case { label: "unsigned-division", primitives: UNSIGNED_DIVISION_PRIMITIVES, source: UNSIGNED_DIVISION_SOURCE, expected: 5 },
    Case { label: "int64", primitives: INT64_PRIMITIVES, source: INT64_SOURCE, expected: 23 },
    Case { label: "uint64", primitives: UINT64_PRIMITIVES, source: UINT64_SOURCE, expected: 31 },
    Case { label: "bitwise", primitives: BITWISE_PRIMITIVES, source: BITWISE_SOURCE, expected: 29 },
    Case { label: "wide-bitwise", primitives: WIDE_BITWISE_PRIMITIVES, source: WIDE_BITWISE_SOURCE, expected: 37 },
];

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

fn build_case(case: &Case, target: Target) -> (BuildOutcome, PathBuf) {
    let label = target.label();
    let root = std::env::temp_dir().join(format!("legion-numeric-{}-{label}-{}", case.label, std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let project_dir = write_project_with(&root, label, case.source, &case.primitive_source(target));
    let output_dir = root.join("dist").join(label);
    let outcome = build_project(&BuildOptions { project_dir, target: Some(label.to_string()), output_dir: Some(output_dir) });
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

/// 冻结入口的实际返回值（宿主投影差异，不是算子能力差异）。
///
/// 三个宿主的投影方式不同：jvm launcher 打印返回值且自身退出码为 0；clr / wasm-node
/// 直接把返回值作为进程退出码；ovm 由解释器进程内调用入口。这是宿主投影差异，
/// 不是算子能力差异——能力差异只体现在夹具声明的绑定上。
fn observed_return(target: Target, outcome: &BuildOutcome) -> i32 {
    match target {
        Target::Jvm => {
            let (code, stdout, stderr) = run_contract(outcome);
            assert_eq!(code, Some(0), "jvm: 入口必须正常退出: stdout={stdout:?} stderr={stderr:?}");
            stdout.trim().parse().unwrap_or_else(|error| panic!("jvm: stdout 必须是入口返回值: stdout={stdout:?} error={error}"))
        }
        Target::Clr | Target::Wasm => {
            let (code, stdout, stderr) = run_contract(outcome);
            code.unwrap_or_else(|| panic!("{}: 缺少退出码: stdout={stdout:?} stderr={stderr:?}", target.label()))
        }
        Target::Ovm => {
            let artifact = outcome
                .artifacts
                .iter()
                .find(|artifact| artifact.ends_with(".odin"))
                .unwrap_or_else(|| panic!("ovm: 缺少 `.odin` 产物: {:?}", outcome.artifacts));
            let bytes = fs::read(artifact).unwrap_or_else(|error| panic!("ovm: 读取 `{artifact}` 失败: {error}"));
            let entry = outcome.entry_symbol.clone().expect("ovm: 构建结果必须给出入口符号");
            let mut vm = OdinVm::new();
            let loaded = vm.load(&bytes).unwrap_or_else(|error| panic!("ovm: 装载 `main.odin` 失败: {error}"));
            match vm.run(&loaded, &entry, Vec::new()).unwrap_or_else(|error| panic!("ovm: 入口 `{entry}` 必须执行到完成: {error}")) {
                Value::I32(value) => value,
                other => panic!("ovm: 入口 `{entry}` 必须返回 `i32`，实际 {other:?}"),
            }
        }
    }
}

/// 数值算子用例在「声明了全部所需绑定」的 target 上真实编译并运行，报告同一结果。
///
/// 可运行 target 集合由夹具声明推导：`f64` 没有 ovm 绑定（VM 无 f64 头码），
/// `u32` 除/余没有 ovm 绑定（VM 无无符号除余头码），因此这两个用例只进
/// jvm / clr / wasm；补齐头码后无需改测试就会自动纳入 ovm。
#[test]
fn numeric_operator_cases_run_on_every_declaring_target() {
    for case in NUMERIC_CASES {
        let targets: Vec<Target> = TARGETS.iter().copied().filter(|target| case.supports(*target)).collect();
        assert!(!targets.is_empty(), "{}: 至少一个 target 必须声明全部所需运算符", case.label);
        for target in targets {
            let (outcome, root) = build_case(case, target);
            assert!(outcome.ok, "{} @ {}: build 失败: {:?}", case.label, target.label(), outcome.error);
            let observed = observed_return(target, &outcome);
            assert_eq!(observed, case.expected, "{} @ {}: 入口返回值必须是期望值", case.label, target.label());
            let _ = fs::remove_dir_all(&root);
        }
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
