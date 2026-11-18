//! 已解码的 WebAssembly 指令子集。
//!
//! 模块二进制与段解析由格式层拥有；本枚举只覆盖骨架可执行的最小集合。

/// 一条已解码指令。
#[derive(Debug, Clone, PartialEq)]
pub enum Instruction {
    /// `nop`
    Nop,
    /// `i32.const`
    I32Const(i32),
    /// `i64.const`
    I64Const(i64),
    /// `local.get`
    LocalGet(usize),
    /// `local.set`
    LocalSet(usize),
    /// `local.tee`
    LocalTee(usize),
    /// `drop`
    Drop,
    /// `i32.add`
    I32Add,
    /// `i32.sub`
    I32Sub,
    /// `i32.mul`
    I32Mul,
    /// `i32.div_s`
    I32DivS,
    /// `i64.add`
    I64Add,
    /// `i64.extend_i32_s`
    I64ExtendI32S,
    /// `return`
    Return,
}
