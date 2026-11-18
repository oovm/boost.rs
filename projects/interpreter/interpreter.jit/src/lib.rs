#![deny(missing_docs)]

//! Optional JIT acceleration substrate for interpreter runtimes.
//!
//! 语言无关：只消费已验证执行格式的字节码视图（[`JitCompileRequest`]）。
//! 格式适配器与通用调度分离：标量模式匹配经 [`ScalarProgramMatcher`] 由目标 VM 注入，
//! 本 crate 不依赖任何具体语言、VM 或字节码格式 crate。
//!
//! 机器码合同见 [`machine_code`]：版本化的可解释 / 可降低中间字节，**不是** OS
//! 可执行页上的原生指令；真实原生发射属于 `compiler-emitter` 的后续闭包。
//! 保守 GC stack map 见 [`stack_map`]，去优化帧恢复合同见 [`deopt`]，
//! 可失效编译假设见 [`assumption`]。

mod artifact;
mod assumption;
mod baseline;
mod compiler;
mod deopt;
mod error;
mod machine_code;
mod request;
mod stack_map;

pub use artifact::JitCompiledArtifact;
pub use assumption::{JitAssumption, baseline_scalar_assumptions};
pub use baseline::{BaselineScalarJit, ScalarProgramMatcher};
pub use compiler::{DisabledJit, JitCompiler, StackMapJit};
pub use deopt::{
    DeoptFrame, DeoptMap, DeoptMapEntry, DeoptRestoreError, InlineFrameSpec, RestoredInterpreterFrame, RestoredLocal, build_baseline_deopt_map,
    build_inline_deopt_map, materialize_interpreter_frames,
};
pub use error::JitError;
pub use machine_code::{
    I32Binop, I32Cmp, IMM_ON_LEFT, IMM_ON_RIGHT, MACHINE_CODE_MAGIC, MachineCodeError, SELECT_ELSE_IMM, SELECT_THEN_IMM, ScalarProgram,
    SelectArm, decode_scalar_program, encode_ret_const_i32, encode_ret_i32_binop_imm_local, encode_ret_i32_binop_locals,
    encode_ret_i32_cmp_imm_local, encode_ret_i32_cmp_locals, encode_ret_i32_select_cmp_consts, encode_ret_i32_select_cmp_locals,
    encode_ret_i32_select_cmp_mixed, encode_ret_local, encode_ret_void,
};
pub use request::{JitCompileRequest, JitFunctionSpec};
pub use stack_map::{FunctionStackMaps, StackMapEntry, build_conservative_stack_maps};
