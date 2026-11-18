#![deny(missing_docs)]
#![forbid(unsafe_code)]

//! WebAssembly 目标执行基座（骨架）。
//!
//! # 树位置
//!
//! 本 crate 是 `interpreter.vm` 树的 Wasm 目标，与 `.ovm`、`.jvm` 同级且互不依赖。
//!
//! # 当前边界
//!
//! - 只接受**已解码**的指令序列；模块二进制、段布局与类型段由格式层（Acorn）拥有。
//! - 已实现：数值栈执行、局部变量、有界执行循环、栈深上限，以及
//!   WASI preview1 与 component-model 的**导入名解析与能力分类合同**。
//! - **未实现**：完整指令集、内存/表/全局实例化、异常与 trap 语义、浮点 NaN 规范化、
//!   以及任何 WASI syscall 的实际实现。宿主边界只做名称与方向判定，不执行 I/O。
//! - 骨架不签收 WASI 或 component-model 符合性；后续实现必须逐项补证据。

pub mod decode_adapter;
pub mod error;
pub mod host;
pub mod op;
pub mod value;
pub mod vm;
pub mod wasi;

pub use decode_adapter::{from_decoded_instruction, from_decoded_program};
pub use error::WasmError;
pub use host::{HostBoundary, ImportKind, classify};
pub use op::Instruction;
pub use value::WasmValue;
pub use vm::{StepReport, WasmVm};
pub use wasi::{ComponentDirection, WasiPreview1Import, component_direction, is_wasi_preview1_import, wasi_preview1_import};
