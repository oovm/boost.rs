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
#   * CLR 的 f32 形参算术/比较已冻结（ECMA 通用 opcode 对 `float32` 载体同形），
#     缺的只是常量载体 `ldc.r4`/`conv.r4`（`f64.rem` 已可编码为 IL `rem`）；
#   * Wasm 无 `f64.rem` / `f32.rem`（wasm 只有整数 rem，没有浮点余数指令）；
#   * OVM 只覆盖 i32 与 f32，没有 i64 / f64 的算术、位运算与比较头码；u32 的 `/`、`%`
#     也没有无符号除余头码（`i32.div`/`i32.rem` 都是有符号）；f32 只有 `eq`、`lt`。
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

    [infix]
    [jvm_operator("integer.compare_less_or_equal")]
    [clr_operator("i32.le")]
    [wasm_operator("i32.le_s")]
    [ovm_operator("i32.le")]
    micro `<=`(self: i32, other: i32) -> bool;

    [infix]
    [jvm_operator("integer.compare_greater_than")]
    [clr_operator("i32.gt")]
    [wasm_operator("i32.gt_s")]
    [ovm_operator("i32.gt")]
    micro `>`(self: i32, other: i32) -> bool;

    [infix]
    [jvm_operator("integer.compare_greater_or_equal")]
    [clr_operator("i32.ge")]
    [wasm_operator("i32.ge_s")]
    [ovm_operator("i32.ge")]
    micro `>=`(self: i32, other: i32) -> bool;

    [infix]
    [jvm_operator("integer.and")]
    [clr_operator("i32.and")]
    [wasm_operator("i32.and")]
    [ovm_operator("i32.and")]
    micro `&`(self: i32, other: i32) -> i32;

    [infix]
    [jvm_operator("integer.or")]
    [clr_operator("i32.or")]
    [wasm_operator("i32.or")]
    [ovm_operator("i32.or")]
    micro `|`(self: i32, other: i32) -> i32;

    [infix]
    [jvm_operator("integer.xor")]
    [clr_operator("i32.xor")]
    [wasm_operator("i32.xor")]
    [ovm_operator("i32.xor")]
    micro `^`(self: i32, other: i32) -> i32;

    [infix]
    [jvm_operator("integer.shift_left")]
    [clr_operator("i32.shl")]
    [wasm_operator("i32.shl")]
    [ovm_operator("i32.shl")]
    micro `<<`(self: i32, other: i32) -> i32;

    [infix]
    [jvm_operator("integer.shift_right")]
    [clr_operator("i32.shr")]
    [wasm_operator("i32.shr_s")]
    [ovm_operator("i32.shr_s")]
    micro `>>`(self: i32, other: i32) -> i32;
}

# 无符号 32 位：加减乘与有符号同形（二进制补码），除/余与有序比较必须用无符号助记符。
# OVM 没有无符号除余头码，故 `/`、`%` 不声明 ovm 绑定。
[primitive("std::primitive::u32")]
structure u32 {
    [infix]
    [jvm_operator("integer.add")]
    [clr_operator("i32.add")]
    [wasm_operator("i32.add")]
    [ovm_operator("i32.add")]
    micro `+`(self: u32, other: u32) -> u32;

    [infix]
    [jvm_operator("integer.subtract")]
    [clr_operator("i32.sub")]
    [wasm_operator("i32.sub")]
    [ovm_operator("i32.sub")]
    micro `-`(self: u32, other: u32) -> u32;

    [infix]
    [jvm_operator("integer.multiply")]
    [clr_operator("i32.mul")]
    [wasm_operator("i32.mul")]
    [ovm_operator("i32.mul")]
    micro `*`(self: u32, other: u32) -> u32;

    [infix]
    [jvm_operator("integer.divide_unsigned")]
    [clr_operator("i32.div.un")]
    [wasm_operator("i32.div_u")]
    micro `/`(self: u32, other: u32) -> u32;

    [infix]
    [jvm_operator("integer.remainder_unsigned")]
    [clr_operator("i32.rem.un")]
    [wasm_operator("i32.rem_u")]
    micro `%`(self: u32, other: u32) -> u32;

    [prefix]
    [jvm_operator("integer.negate")]
    [clr_operator("i32.neg")]
    [wasm_operator("i32.neg")]
    [ovm_operator("i32.neg")]
    micro `-`(self: u32) -> u32;

    [infix]
    [jvm_operator("integer.compare_equal")]
    [clr_operator("i32.eq")]
    [wasm_operator("i32.eq")]
    [ovm_operator("i32.eq")]
    micro `==`(self: u32, other: u32) -> bool;

    [infix]
    [jvm_operator("integer.compare_not_equal")]
    [clr_operator("i32.ne")]
    [wasm_operator("i32.ne")]
    [ovm_operator("i32.ne")]
    micro `!=`(self: u32, other: u32) -> bool;

    [infix]
    [jvm_operator("integer.compare_less_than_unsigned")]
    [clr_operator("i32.lt.un")]
    [wasm_operator("i32.lt_u")]
    [ovm_operator("i32.lt_u")]
    micro `<`(self: u32, other: u32) -> bool;

    [infix]
    [jvm_operator("integer.compare_less_or_equal_unsigned")]
    [clr_operator("i32.le.un")]
    [wasm_operator("i32.le_u")]
    [ovm_operator("i32.le_u")]
    micro `<=`(self: u32, other: u32) -> bool;

    [infix]
    [jvm_operator("integer.compare_greater_than_unsigned")]
    [clr_operator("i32.gt.un")]
    [wasm_operator("i32.gt_u")]
    [ovm_operator("i32.gt_u")]
    micro `>`(self: u32, other: u32) -> bool;

    [infix]
    [jvm_operator("integer.compare_greater_or_equal_unsigned")]
    [clr_operator("i32.ge.un")]
    [wasm_operator("i32.ge_u")]
    [ovm_operator("i32.ge_u")]
    micro `>=`(self: u32, other: u32) -> bool;

    [infix]
    [jvm_operator("integer.and")]
    [clr_operator("i32.and")]
    [wasm_operator("i32.and")]
    [ovm_operator("i32.and")]
    micro `&`(self: u32, other: u32) -> u32;

    [infix]
    [jvm_operator("integer.or")]
    [clr_operator("i32.or")]
    [wasm_operator("i32.or")]
    [ovm_operator("i32.or")]
    micro `|`(self: u32, other: u32) -> u32;

    [infix]
    [jvm_operator("integer.xor")]
    [clr_operator("i32.xor")]
    [wasm_operator("i32.xor")]
    [ovm_operator("i32.xor")]
    micro `^`(self: u32, other: u32) -> u32;

    [infix]
    [jvm_operator("integer.shift_left")]
    [clr_operator("i32.shl")]
    [wasm_operator("i32.shl")]
    [ovm_operator("i32.shl")]
    micro `<<`(self: u32, other: u32) -> u32;

    # `u32` 右移必须是逻辑右移（`i32.shr_u`/`i32.shr.un`/`iushr`），不得借有符号 `>>`。
    [infix]
    [jvm_operator("integer.shift_right_unsigned")]
    [clr_operator("i32.shr.un")]
    [wasm_operator("i32.shr_u")]
    [ovm_operator("i32.shr_u")]
    micro `>>`(self: u32, other: u32) -> u32;
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

    [infix]
    [jvm_operator("long.compare_less_or_equal")]
    [clr_operator("i64.le")]
    [wasm_operator("i64.le_s")]
    micro `<=`(self: i64, other: i64) -> bool;

    [infix]
    [jvm_operator("long.compare_greater_than")]
    [clr_operator("i64.gt")]
    [wasm_operator("i64.gt_s")]
    micro `>`(self: i64, other: i64) -> bool;

    [infix]
    [jvm_operator("long.compare_greater_or_equal")]
    [clr_operator("i64.ge")]
    [wasm_operator("i64.ge_s")]
    micro `>=`(self: i64, other: i64) -> bool;

    [infix]
    [jvm_operator("long.and")]
    [clr_operator("i64.and")]
    [wasm_operator("i64.and")]
    micro `&`(self: i64, other: i64) -> i64;

    [infix]
    [jvm_operator("long.or")]
    [clr_operator("i64.or")]
    [wasm_operator("i64.or")]
    micro `|`(self: i64, other: i64) -> i64;

    [infix]
    [jvm_operator("long.xor")]
    [clr_operator("i64.xor")]
    [wasm_operator("i64.xor")]
    micro `^`(self: i64, other: i64) -> i64;

    [infix]
    [jvm_operator("long.shift_left")]
    [clr_operator("i64.shl")]
    [wasm_operator("i64.shl")]
    micro `<<`(self: i64, other: i64) -> i64;

    # 宽载体右移：JVM/CLR 由冻结配方先把位移量截窄到 i32，Wasm 的 `i64.shr_s` 直接收 i64。
    [infix]
    [jvm_operator("long.shift_right")]
    [clr_operator("i64.shr")]
    [wasm_operator("i64.shr_s")]
    micro `>>`(self: i64, other: i64) -> i64;
}

# 无符号 64 位：OVM 没有任何 i64 头码，因此不含 ovm 绑定。
[primitive("std::primitive::u64")]
structure u64 {
    [infix]
    [jvm_operator("long.add")]
    [clr_operator("i64.add")]
    [wasm_operator("i64.add")]
    micro `+`(self: u64, other: u64) -> u64;

    [infix]
    [jvm_operator("long.subtract")]
    [clr_operator("i64.sub")]
    [wasm_operator("i64.sub")]
    micro `-`(self: u64, other: u64) -> u64;

    [infix]
    [jvm_operator("long.multiply")]
    [clr_operator("i64.mul")]
    [wasm_operator("i64.mul")]
    micro `*`(self: u64, other: u64) -> u64;

    [infix]
    [jvm_operator("long.divide_unsigned")]
    [clr_operator("i64.div.un")]
    [wasm_operator("i64.div_u")]
    micro `/`(self: u64, other: u64) -> u64;

    [infix]
    [jvm_operator("long.remainder_unsigned")]
    [clr_operator("i64.rem.un")]
    [wasm_operator("i64.rem_u")]
    micro `%`(self: u64, other: u64) -> u64;

    [prefix]
    [jvm_operator("long.negate")]
    [clr_operator("i64.neg")]
    [wasm_operator("i64.neg")]
    micro `-`(self: u64) -> u64;

    [infix]
    [jvm_operator("long.compare_equal")]
    [clr_operator("i64.eq")]
    [wasm_operator("i64.eq")]
    micro `==`(self: u64, other: u64) -> bool;

    [infix]
    [jvm_operator("long.compare_not_equal")]
    [clr_operator("i64.ne")]
    [wasm_operator("i64.ne")]
    micro `!=`(self: u64, other: u64) -> bool;

    [infix]
    [jvm_operator("long.compare_less_than_unsigned")]
    [clr_operator("i64.lt.un")]
    [wasm_operator("i64.lt_u")]
    micro `<`(self: u64, other: u64) -> bool;

    [infix]
    [jvm_operator("long.compare_less_or_equal_unsigned")]
    [clr_operator("i64.le.un")]
    [wasm_operator("i64.le_u")]
    micro `<=`(self: u64, other: u64) -> bool;

    [infix]
    [jvm_operator("long.compare_greater_than_unsigned")]
    [clr_operator("i64.gt.un")]
    [wasm_operator("i64.gt_u")]
    micro `>`(self: u64, other: u64) -> bool;

    [infix]
    [jvm_operator("long.compare_greater_or_equal_unsigned")]
    [clr_operator("i64.ge.un")]
    [wasm_operator("i64.ge_u")]
    micro `>=`(self: u64, other: u64) -> bool;

    [infix]
    [jvm_operator("long.and")]
    [clr_operator("i64.and")]
    [wasm_operator("i64.and")]
    micro `&`(self: u64, other: u64) -> u64;

    [infix]
    [jvm_operator("long.or")]
    [clr_operator("i64.or")]
    [wasm_operator("i64.or")]
    micro `|`(self: u64, other: u64) -> u64;

    [infix]
    [jvm_operator("long.xor")]
    [clr_operator("i64.xor")]
    [wasm_operator("i64.xor")]
    micro `^`(self: u64, other: u64) -> u64;

    [infix]
    [jvm_operator("long.shift_left")]
    [clr_operator("i64.shl")]
    [wasm_operator("i64.shl")]
    micro `<<`(self: u64, other: u64) -> u64;

    # `u64` 右移必须是逻辑右移（`i64.shr_u`/`i64.shr.un`/`lushr`）。
    [infix]
    [jvm_operator("long.shift_right_unsigned")]
    [clr_operator("i64.shr.un")]
    [wasm_operator("i64.shr_u")]
    micro `>>`(self: u64, other: u64) -> u64;
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
    [clr_operator("f64.rem")]
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

    [infix]
    [jvm_operator("double.compare_less_or_equal")]
    [clr_operator("f64.le")]
    [wasm_operator("f64.le")]
    micro `<=`(self: f64, other: f64) -> bool;

    [infix]
    [jvm_operator("double.compare_greater_than")]
    [clr_operator("f64.gt")]
    [wasm_operator("f64.gt")]
    micro `>`(self: f64, other: f64) -> bool;

    [infix]
    [jvm_operator("double.compare_greater_or_equal")]
    [clr_operator("f64.ge")]
    [wasm_operator("f64.ge")]
    micro `>=`(self: f64, other: f64) -> bool;
}

# 单精度 32 位浮点：载体是独立的 `F32`，不得借 f64/i32 槽中转。
# CLR 的形参算术/比较走 ECMA 通用 opcode；仍缺 `ldc.r4` 常量载体，f32 常量路径
# 在 emit 侧失败闭合。OVM 只有 `eq`/`lt` 头码，且没有浮点余数指令；
# Wasm 没有 `f32.rem`。
[primitive("std::primitive::f32")]
structure f32 {
    [infix]
    [jvm_operator("float.add")]
    [clr_operator("f32.add")]
    [wasm_operator("f32.add")]
    [ovm_operator("f32.add")]
    micro `+`(self: f32, other: f32) -> f32;

    [infix]
    [jvm_operator("float.subtract")]
    [clr_operator("f32.sub")]
    [wasm_operator("f32.sub")]
    [ovm_operator("f32.sub")]
    micro `-`(self: f32, other: f32) -> f32;

    [infix]
    [jvm_operator("float.multiply")]
    [clr_operator("f32.mul")]
    [wasm_operator("f32.mul")]
    [ovm_operator("f32.mul")]
    micro `*`(self: f32, other: f32) -> f32;

    [infix]
    [jvm_operator("float.divide")]
    [clr_operator("f32.div")]
    [wasm_operator("f32.div")]
    [ovm_operator("f32.div")]
    micro `/`(self: f32, other: f32) -> f32;

    [infix]
    [jvm_operator("float.remainder")]
    [clr_operator("f32.rem")]
    micro `%`(self: f32, other: f32) -> f32;

    [prefix]
    [jvm_operator("float.negate")]
    [clr_operator("f32.neg")]
    [wasm_operator("f32.neg")]
    [ovm_operator("f32.neg")]
    micro `-`(self: f32) -> f32;

    [infix]
    [jvm_operator("float.compare_equal")]
    [clr_operator("f32.eq")]
    [wasm_operator("f32.eq")]
    [ovm_operator("f32.eq")]
    micro `==`(self: f32, other: f32) -> bool;

    [infix]
    [jvm_operator("float.compare_not_equal")]
    [clr_operator("f32.ne")]
    [wasm_operator("f32.ne")]
    micro `!=`(self: f32, other: f32) -> bool;

    [infix]
    [jvm_operator("float.compare_less_than")]
    [clr_operator("f32.lt")]
    [wasm_operator("f32.lt")]
    [ovm_operator("f32.lt")]
    micro `<`(self: f32, other: f32) -> bool;

    [infix]
    [jvm_operator("float.compare_less_or_equal")]
    [clr_operator("f32.le")]
    [wasm_operator("f32.le")]
    micro `<=`(self: f32, other: f32) -> bool;

    [infix]
    [jvm_operator("float.compare_greater_than")]
    [clr_operator("f32.gt")]
    [wasm_operator("f32.gt")]
    micro `>`(self: f32, other: f32) -> bool;

    [infix]
    [jvm_operator("float.compare_greater_or_equal")]
    [clr_operator("f32.ge")]
    [wasm_operator("f32.ge")]
    micro `>=`(self: f32, other: f32) -> bool;
}

[primitive("std::primitive::bool")]
structure bool { }
