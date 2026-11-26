//! 残差 IR：目标无关的栈式语义操作。
//!
//! 特化（第一投影）的输出。这里没有任何目标形状：不含 Odin / OVM 头码，
//! 不含 `ts.*` provider 符号名，也不含任何目标常量池 / 布局编码细节。
//! 操作按栈式语义命名（数值算术、文本拼接、truthiness、对象字段读写、
//! 控制流标签），由语言 × 目标适配器映射到具体目标的低层 IR。

/// 布局身份（注册序）。
pub type LayoutId = u32;

/// 帧槽身份。
pub type SlotId = u32;

/// 函数内标签身份。
pub type LabelId = u32;

/// 残差常量。
#[derive(Debug, Clone, PartialEq)]
pub enum ResidualConst {
    /// `undefined`。
    Undefined,
    /// `null`。
    Null,
    /// 布尔。
    Bool(bool),
    /// 数字。
    Number(f64),
    /// 字符串。
    String(String),
}

/// 残差操作（栈式语义）。
#[derive(Debug, Clone, PartialEq)]
pub enum ResidualOp {
    /// 压入常量。
    Const(ResidualConst),
    /// 压入帧槽值。
    Load(SlotId),
    /// 弹出栈顶写入帧槽。
    Store(SlotId),
    /// 丢弃栈顶。
    Pop,
    /// 复制栈顶。
    Dup,
    /// 数值加。
    NumAdd,
    /// 数值减。
    NumSub,
    /// 数值乘。
    NumMul,
    /// 数值除。
    NumDiv,
    /// 数值取负。
    NumNeg,
    /// 文本拼接。
    StrConcat,
    /// 数值 → 文本。
    NumToText,
    /// 动态加（JS `+` 语义，类型不可静态判定）。
    DynamicAdd,
    /// 动态 truthiness。
    Truthy,
    /// 布尔取反。
    BoolNot,
    /// 数值小于。
    NumLt,
    /// 数值小于等于。
    NumLe,
    /// 数值大于。
    NumGt,
    /// 数值大于等于。
    NumGe,
    /// 文本相等。
    StrEq,
    /// 数值相等。
    NumEq,
    /// 动态严格相等（含引用身份）。
    DynamicStrictEq,
    /// 调用函数表项（实参先压栈）。
    Call(usize),
    /// 按布局新建对象并压栈。
    ObjectNew(LayoutId),
    /// 字段写入：栈 `[object, value]` → 写后留下 `object`。
    FieldSet(LayoutId, u32),
    /// 字段读取：栈 `[object]` → 留下字段值。
    FieldGet(LayoutId, u32),
    /// 宿主日志（语言侧效应身份 `console.log` 的残差）。
    HostLog {
        /// 实参个数。
        arity: u32,
    },
    /// 标签定义。
    Label(LabelId),
    /// 栈顶为假跳转。
    JumpIfFalse(LabelId),
    /// 栈顶为真跳转。
    JumpIfTrue(LabelId),
    /// 无条件跳转。
    Jump(LabelId),
    /// 返回栈顶。
    Return,
}

/// 一个函数的残差体。
#[derive(Debug, Clone)]
pub struct ResidualFunction {
    /// 函数名。
    pub name: String,
    /// 形参个数（占槽 `0..arity`）。
    pub arity: u32,
    /// 帧槽总数。
    pub slots: u32,
    /// 残差操作序列。
    pub body: Vec<ResidualOp>,
    /// 是否导出（产物入口）。
    pub exported: bool,
}

/// 残差程序。
#[derive(Debug, Clone)]
pub struct ResidualProgram {
    /// 函数残差（函数表序）。
    pub functions: Vec<ResidualFunction>,
    /// 布局注册序 → 冻结字段名（按名字排序去重）。
    pub layouts: Vec<Vec<String>>,
}
