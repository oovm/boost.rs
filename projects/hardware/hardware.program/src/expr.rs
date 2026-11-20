use serde::{Deserialize, Serialize};

/// 二元运算（L0）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Min,
    Max,
    Mod,
    Pow,
    Dot,
    Lt,
    Le,
    Eq,
    Ne,
    Ge,
    Gt,
    /// 四象限角度，操作数依次为 y、x，返回弧度。输入须为同精度浮点标量。
    Atan2,
    /// u32 按位与。两个操作数都必须是 `u32` 标量。
    And,
    /// u32 按位或。两个操作数都必须是 `u32` 标量。
    Or,
    /// u32 逻辑左移。移位量 >= 32 时结果为 0。
    Shl,
    /// u32 逻辑右移。移位量 >= 32 时结果为 0。
    Shr,
}

/// `F2I` 转换的显式舍入模式。语义与 Rust CPU 参考（`f32::round` / `as` 饱和转换）对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Rounding {
    /// 向零舍入（C 风格截断），随后饱和到 `u32` 范围。
    Trunc,
    /// 就近舍入、平局远离零（与 Rust `f32::round` 一致），随后饱和到 `u32` 范围。
    Round,
}

/// 一元运算（L0）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UnOp {
    Neg,
    Sqrt,
    Sin,
    Cos,
    Atan,
    Length,
    Normalize,
    Abs,
    Ln,
    /// 判断标量是否既不是 NaN 也不是无穷大。
    IsFinite,
    /// 取向量分量：0=x，1=y，2=z。
    Vec3Comp(u8),
    /// u32 与 f32 之间的按位重解读，不改变 32 位位模式。
    Bitcast,
    /// u32 到 f32 的精确转换；超出 2^24 精确范围按 IEEE 就近取偶舍入。
    U32ToF32,
    /// f32 到 u32 的转换，带显式舍入模式；结果饱和（负数与 NaN 得 0，超界得 u32::MAX）。
    F32ToU32(Rounding),
}

/// 表达式节点。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Expr {
    ConstF32(f32),
    ConstF64(f64),
    ConstBool(bool),
    ConstU32(u32),
    /// 局部槽位（函数内 `Let` 绑定）。
    LoadLocal(u32),
    /// Uniform 槽位（内核绑定期填入）。
    LoadUniform(u32),
    /// 从逻辑资源 buffer 读取一个元素。
    LoadBuffer {
        resource: u32,
        index: Box<Expr>,
    },
    /// 并行实例全局索引（L2）。
    GlobalId,
    BinOp {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    UnOp {
        op: UnOp,
        arg: Box<Expr>,
    },
    Select {
        cond: Box<Expr>,
        then_: Box<Expr>,
        else_: Box<Expr>,
    },
    /// 由三个标量分量构造 `Vec3`。
    Vec3Compose {
        x: Box<Expr>,
        y: Box<Expr>,
        z: Box<Expr>,
    },
    Call {
        func: u32,
        args: Vec<Expr>,
    },
}
