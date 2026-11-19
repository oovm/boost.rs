//! 自研 x86-64 指令编码层，服务 Windows MSVC native 后端。
//! 提供最小 `X64Instruction` IR 与两遍编码器，封装 Microsoft x64 调用约定辅助
//! （shadow space、参数寄存器）。只负责机器码字节，不写 COFF/PE 容器，不承载语言语义。

mod abi_msvc;
mod abi_sysv;
mod encode;
mod instruction;
mod layout;
mod reg;

pub use abi_msvc::{MsvcFunctionBuilder, SHADOW_STACK_SIZE};
pub use abi_sysv::SysvFunctionBuilder;
pub use encode::{EncodedModule, X64Encoder, X64Fixup, X64FixupKind, apply_fixups};
pub use instruction::{ConditionCode, X64Instruction};
pub use layout::ObjectLayout;
pub use reg::Reg64;
