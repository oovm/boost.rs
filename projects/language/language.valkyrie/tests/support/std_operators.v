# 语言测试夹具：标量运算符作为普通成员声明的标准库源。
#
# 运算符不是语义类别：`a + b` 等价于 `a.infix +(b)`，`a < b` 等价于 `a.infix <(b)`。
# 每个运算符都是结构体内的普通 `micro` 成员，无函数体（`;` 结尾）且携带目标绑定注解，
# HIR 因此把声明标记为 abstract 并由目标绑定生成外部导入。
#
# 助记符词汇以各 target 的冻结函数为准，不得照抄注释：
#   JVM  freeze_jvm_operator_binding   (compiler.emitter/src/lowering/backends/jvm/recipe.rs)
#   CLR  freeze_clr_operator_binding   (compiler.emitter/src/lowering/backends/clr/mir.rs)
#   Wasm freeze_wasm_numeric_binding   (compiler.emitter/src/lowering/backends/wasm/mir/numeric.rs)
#   OVM  freeze_ovm_operator_binding   (compiler.emitter/src/lowering/backends/ovm/mir.rs)
#
# 配方缺口（不编造助记符）：
#   * JVM 无 `double.compare_less_or_equal` / `compare_greater_than` /
#     `compare_greater_or_equal`（夹具未声明这些成员）；
#   * CLR 无 `f64.rem`（.NET 没有与 Java `%`/fmod 语义一致的 IL 指令，
#     `Math.IEEERemainder` 是 IEEE 754 remainder，语义不同）；
#   * Wasm 无 `f64.rem`（wasm 只有整数 rem，没有浮点余数指令）；
#   * OVM 只覆盖 i32 与 f32，没有 i64 / f64 的算术与比较头码。
# 因此某些 (类型, 运算, target) 组合不会出现该 target 的注解。

namespace std.primitive;

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

[primitive("std::primitive::i64")]
structure i64 {
    [infix]
    [jvm_operator("long.add")]
    [clr_operator("i64.add")]
    [wasm_operator("i64.add")]
    micro `+`(self: i64, other: i64) -> i64;

    [infix]
    [jvm_operator("long.subtract")]
    [clr_operator("i64.sub")]
    [wasm_operator("i64.sub")]
    micro `-`(self: i64, other: i64) -> i64;

    [infix]
    [jvm_operator("long.multiply")]
    [clr_operator("i64.mul")]
    [wasm_operator("i64.mul")]
    micro `*`(self: i64, other: i64) -> i64;

    [infix]
    [jvm_operator("long.divide")]
    [clr_operator("i64.div")]
    [wasm_operator("i64.div_s")]
    micro `/`(self: i64, other: i64) -> i64;

    [infix]
    [jvm_operator("long.remainder")]
    [clr_operator("i64.rem")]
    [wasm_operator("i64.rem_s")]
    micro `%`(self: i64, other: i64) -> i64;

    [prefix]
    [jvm_operator("long.negate")]
    [clr_operator("i64.neg")]
    [wasm_operator("i64.neg")]
    micro `-`(self: i64) -> i64;

    [infix]
    [jvm_operator("long.compare_equal")]
    [clr_operator("i64.eq")]
    [wasm_operator("i64.eq")]
    micro `==`(self: i64, other: i64) -> bool;

    [infix]
    [jvm_operator("long.compare_not_equal")]
    [clr_operator("i64.ne")]
    [wasm_operator("i64.ne")]
    micro `!=`(self: i64, other: i64) -> bool;

    [infix]
    [jvm_operator("long.compare_less_than")]
    [clr_operator("i64.lt")]
    [wasm_operator("i64.lt_s")]
    micro `<`(self: i64, other: i64) -> bool;
}

[primitive("std::primitive::f64")]
structure f64 {
    [infix]
    [jvm_operator("double.add")]
    [clr_operator("f64.add")]
    [wasm_operator("f64.add")]
    micro `+`(self: f64, other: f64) -> f64;

    [infix]
    [jvm_operator("double.subtract")]
    [clr_operator("f64.sub")]
    [wasm_operator("f64.sub")]
    micro `-`(self: f64, other: f64) -> f64;

    [infix]
    [jvm_operator("double.multiply")]
    [clr_operator("f64.mul")]
    [wasm_operator("f64.mul")]
    micro `*`(self: f64, other: f64) -> f64;

    [infix]
    [jvm_operator("double.divide")]
    [clr_operator("f64.div")]
    [wasm_operator("f64.div")]
    micro `/`(self: f64, other: f64) -> f64;

    [infix]
    [jvm_operator("double.remainder")]
    micro `%`(self: f64, other: f64) -> f64;

    [prefix]
    [jvm_operator("double.negate")]
    [clr_operator("f64.neg")]
    [wasm_operator("f64.neg")]
    micro `-`(self: f64) -> f64;

    [infix]
    [jvm_operator("double.compare_equal")]
    [clr_operator("f64.eq")]
    [wasm_operator("f64.eq")]
    micro `==`(self: f64, other: f64) -> bool;

    [infix]
    [jvm_operator("double.compare_not_equal")]
    [clr_operator("f64.ne")]
    [wasm_operator("f64.ne")]
    micro `!=`(self: f64, other: f64) -> bool;

    [infix]
    [jvm_operator("double.compare_less_than")]
    [clr_operator("f64.lt")]
    [wasm_operator("f64.lt")]
    micro `<`(self: f64, other: f64) -> bool;
}

[primitive("std::primitive::bool")]
structure bool { }
