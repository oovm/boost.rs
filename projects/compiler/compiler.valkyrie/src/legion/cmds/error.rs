//! Legion 命令链错误：统一由 [`crate::legion::error`] 持有，避免两套错误类型。

pub use crate::legion::error::{LegionError, LegionResult};
