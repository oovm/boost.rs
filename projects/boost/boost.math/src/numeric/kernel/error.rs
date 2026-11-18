//! Machine kernel 与执行预算的失败语义。
//!
//! 本模块不依赖宿主诊断类型：kernel 只报告结构化的 [`KernelError`]，由下游
//! （如科学计算数值塔）映射为自己的诊断。`Display` 输出稳定、可读，便于
//! 宿主在边界处包装成任意错误模型。

use core::fmt;

/// kernel / 预算操作的统一结果别名。
pub type Result<T> = core::result::Result<T, KernelError>;

/// Machine kernel 与 [`ExecutionBudget`](crate::numeric::budget::ExecutionBudget) 的失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelError {
    /// 域错误：操作数不满足前置条件（除零、减法下溢、scratch 越界等）。
    ///
    /// 载荷是稳定的操作标签（如 `div_zero`、`sub_underflow`），不是格式化文本。
    Domain(&'static str),
    /// 资源上限拒绝：请求量 `got` 超过策略上限 `max`。
    ResourceLimit {
        /// 受限资源标签（`limbs`、`significand_bits`、`wire_bytes`）。
        kind: &'static str,
        /// 实际请求量。
        got: usize,
        /// 策略允许的上限。
        max: u32,
    },
}

impl fmt::Display for KernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Domain(operation) => write!(f, "numeric domain error: {operation}"),
            Self::ResourceLimit { kind, got, max } => write!(f, "numeric resource limit: {kind} {got} exceeds {max}"),
        }
    }
}
