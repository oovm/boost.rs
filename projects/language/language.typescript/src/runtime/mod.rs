//! 解释语义层：值域、JS 语义函数与树遍历解释器。
//!
//! 解释器是差分基线，同时是第一投影的语义来源：已知值在特化期走这里的
//! 语义函数结算，动态值残差到 provider，由 provider 复用同一批函数。

pub mod interpreter;
pub mod semantics;
pub mod value;

#[cfg(feature = "ovm-lane")]
pub mod provider;

pub use interpreter::{Interpreter, RuntimeError};
pub use value::Value;
