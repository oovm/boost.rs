//! UCF program and shader ISA encoders.
//!
//! Backends never call individual emitters. They select a [`TargetIsa`] and go
//! through [`encode_shader`] / [`encode_program`], which validate and optimize
//! once before dispatching to exactly one encoder.
#![warn(missing_docs)]

mod codegen;
mod dxil;
mod hip;
mod program_spirv;
mod ptx;
mod spirv;
mod spv;
mod task;

pub use codegen::{
    CodegenError, CodegenOptions, DxbcEncoder, EncodedModule, Encoder, HsacoEncoder, PtxEncoder, SpirvEncoder, TargetIsa, encode_program,
    encode_program_traced, encode_shader, encode_shader_traced, encoder,
};
pub use dxil::{RasterTriDxbc, emit_dxil, emit_raster_tri_dxbc};
pub use hip::{emit_hip, emit_hsaco};
pub use program_spirv::{ProgramSpirvError, emit_program_spirv};
pub use ptx::emit_ptx;
pub use spirv::{RasterTriSpirv, emit_raster_tri_spirv, emit_spirv};
pub use task::{ProgramLowerError, needs_kernel, program_from_task, try_program_from_task};
