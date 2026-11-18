#![deny(missing_docs)]
#![forbid(unsafe_code)]

//! JVM 目标执行基座（骨架）。
//!
//! # 树位置
//!
//! 本 crate 是 `interpreter.vm` 树的 JVM 目标，与 `.ovm`、`.wasm` 同级且互不依赖。
//!
//! # 当前边界
//!
//! - 只接受**已解码**的指令序列；class 文件解析、常量池与属性表由格式层（Acorn）拥有。
//! - 已实现：槽宽正确的局部变量与操作数栈、有界执行循环、native 方法宿主边界。
//! - **未实现**：异常表与 `athrow`、`invokedynamic`、类初始化、验证器的完整数据流分析、
//!   `long`/`double` 之外的对象引用与堆。骨架不签收这些能力，也不得被描述为完整 JVM。`n//! - [`class_adapter`] 只把格式层已解码的指令映射进执行子集，超出子集显式拒绝。
//! - 托管堆与 GC 归 `interpreter-gc`，JIT 归 `interpreter-jit`，本 crate 不重建。

pub mod class_adapter;
pub mod error;
pub mod frame;
pub mod host;
pub mod op;
pub mod vm;

pub use class_adapter::{from_class_instruction, from_class_program};
pub use error::JvmError;
pub use frame::{Frame, Slot};
pub use host::{NativeBinding, NativeRegistry};
pub use op::Instruction;
pub use vm::{Jvm, StepReport};
