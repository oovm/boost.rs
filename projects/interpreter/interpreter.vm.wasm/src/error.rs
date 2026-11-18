//! 执行期错误。

use std::{error::Error, fmt};

/// 执行期错误。
#[derive(Debug, Clone, PartialEq)]
pub enum WasmError {
    /// 数值栈为空。
    StackUnderflow {
        /// 需要的元素数。
        needed: usize,
        /// 实际元素数。
        available: usize,
    },
    /// 数值栈超过上限。
    StackOverflow {
        /// 上限。
        limit: usize,
    },
    /// 局部变量索引越界。
    LocalOutOfRange {
        /// 请求的索引。
        index: usize,
        /// 局部变量数量。
        locals: usize,
    },
    /// 值类型与指令期望不符。
    TypeMismatch {
        /// 期望类型。
        expected: &'static str,
        /// 实际类型。
        actual: &'static str,
    },
    /// 整数除以零。
    DivideByZero,
    /// 步进预算耗尽。
    StepBudgetExceeded {
        /// 预算。
        budget: u64,
    },
    /// 导入名无法解析为已支持的形式。
    UnsupportedImport {
        /// 导入名。
        name: String,
    },
    /// 程序在返回前结束。
    MissingReturn,
}

impl fmt::Display for WasmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StackUnderflow { needed, available } => write!(f, "value stack underflow: needed {needed}, available {available}"),
            Self::StackOverflow { limit } => write!(f, "value stack overflow over {limit} values"),
            Self::LocalOutOfRange { index, locals } => write!(f, "local {index} out of range for {locals} locals"),
            Self::TypeMismatch { expected, actual } => write!(f, "type mismatch: expected {expected}, actual {actual}"),
            Self::DivideByZero => f.write_str("integer divide by zero"),
            Self::StepBudgetExceeded { budget } => write!(f, "step budget {budget} exceeded"),
            Self::UnsupportedImport { name } => write!(f, "unsupported import `{name}`"),
            Self::MissingReturn => f.write_str("function ended without returning"),
        }
    }
}

impl Error for WasmError {}
