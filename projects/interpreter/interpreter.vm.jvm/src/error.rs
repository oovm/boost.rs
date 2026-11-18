//! JVM 目标执行错误。

use std::{error::Error, fmt};

/// 执行期错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JvmError {
    /// 操作数栈为空时出栈或运算。
    StackUnderflow {
        /// 需要的槽数。
        needed: usize,
        /// 实际槽数。
        available: usize,
    },
    /// 操作数栈超过上限。
    StackOverflow {
        /// 上限。
        limit: usize,
    },
    /// 局部变量槽越界。
    LocalOutOfRange {
        /// 请求的槽索引。
        index: usize,
        /// 已分配的槽数。
        slots: usize,
    },
    /// 指令宽度与槽内容不匹配。
    TypeMismatch {
        /// 期望的类别。
        expected: &'static str,
        /// 实际的类别。
        actual: &'static str,
    },
    /// 整数除以零。
    DivideByZero,
    /// 步进预算耗尽。
    StepBudgetExceeded {
        /// 预算。
        budget: u64,
    },
    /// 未注册的 native 方法。
    UnboundNative {
        /// 方法名。
        name: String,
    },
    /// 指令超出当前执行子集。
    UnsupportedInstruction {
        /// 指令的调试形式。
        instruction: String,
    },
    /// 程序在返回前结束。
    MissingReturn,
}

impl fmt::Display for JvmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StackUnderflow { needed, available } => write!(f, "operand stack underflow: needed {needed}, available {available}"),
            Self::StackOverflow { limit } => write!(f, "operand stack overflow over {limit} slots"),
            Self::LocalOutOfRange { index, slots } => write!(f, "local slot {index} out of range for {slots} slots"),
            Self::TypeMismatch { expected, actual } => write!(f, "type mismatch: expected {expected}, actual {actual}"),
            Self::DivideByZero => f.write_str("integer divide by zero"),
            Self::StepBudgetExceeded { budget } => write!(f, "step budget {budget} exceeded"),
            Self::UnboundNative { name } => write!(f, "unbound native method `{name}`"),
            Self::UnsupportedInstruction { instruction } => write!(f, "instruction outside the execution subset: {instruction}"),
            Self::MissingReturn => f.write_str("program ended without returning"),
        }
    }
}

impl Error for JvmError {}
