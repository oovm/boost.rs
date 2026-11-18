//! 已解码的 JVM 指令子集。
//!
//! class 文件解析与常量池由格式层拥有；本枚举只覆盖当前骨架可执行的最小集合。

/// 一条已解码指令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instruction {
    /// `nop`
    Nop,
    /// `iconst_<n>` / `bipush`
    IConst(i32),
    /// `lconst_<n>`
    LConst(i64),
    /// `iload <index>`
    ILoad(usize),
    /// `lload <index>`
    LLoad(usize),
    /// `istore <index>`
    IStore(usize),
    /// `lstore <index>`
    LStore(usize),
    /// `pop`
    Pop,
    /// `iadd`
    IAdd,
    /// `isub`
    ISub,
    /// `imul`
    IMul,
    /// `idiv`
    IDiv,
    /// `ladd`
    LAdd,
    /// `i2l`
    I2L,
    /// `invokestatic` 到已注册的 native 方法。
    InvokeStaticNative {
        /// 方法名。
        name: String,
    },
    /// `ireturn`
    IReturn,
    /// `lreturn`
    LReturn,
    /// `return`
    Return,
}
