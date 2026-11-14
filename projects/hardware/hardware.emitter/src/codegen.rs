//! Unified code generation entry.
//!
//! Every backend asks for bytes through this module. An ISA is selected once,
//! the shared program optimization runs once, and exactly one encoder produces
//! the module. Backends never call individual emitters directly.

use hardware_optimize::{OptimizeOptions, optimize_program};
use hardware_program::{Program, validate_program};
use hardware_types::ShaderProgram;

use crate::{emit_dxil, emit_hsaco, emit_program_spirv, emit_ptx, emit_spirv};

/// Target instruction set architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetIsa {
    /// CUDA PTX text (NUL-terminated).
    Ptx,
    /// Direct3D compute bytecode (`cs_5_1` DXBC on Windows).
    Dxbc,
    /// Vulkan SPIR-V module bytes.
    Spirv,
    /// AMD HSACO ELF code object for `hipModuleLoadData`.
    Hsaco,
}

impl TargetIsa {
    /// Stable lowercase name used in diagnostics.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ptx => "ptx",
            Self::Dxbc => "dxbc",
            Self::Spirv => "spirv",
            Self::Hsaco => "hsaco",
        }
    }
}

/// Options shared by every ISA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodegenOptions {
    /// Run the shared program-level optimization passes before encoding.
    pub optimize: bool,
}

impl Default for CodegenOptions {
    fn default() -> Self {
        Self { optimize: true }
    }
}

/// Bytes produced for one ISA.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedModule {
    /// Selected ISA.
    pub isa: TargetIsa,
    /// Opaque bytecode or text the driver loads.
    pub bytes: Vec<u8>,
}

/// Failure raised by the unified codegen entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodegenError {
    /// The selected ISA has no encoder for this input shape yet.
    Unsupported {
        /// Requested ISA.
        isa: TargetIsa,
        /// Why the input cannot be encoded for that ISA.
        reason: String,
    },
    /// The encoder rejected or failed on the input.
    Encode {
        /// Requested ISA.
        isa: TargetIsa,
        /// Encoder diagnostic.
        message: String,
    },
    /// The program failed validation before encoding.
    Invalid(String),
}

impl std::fmt::Display for CodegenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported { isa, reason } => write!(f, "`{}` codegen is unsupported: {reason}", isa.name()),
            Self::Encode { isa, message } => write!(f, "`{}` codegen failed: {message}", isa.name()),
            Self::Invalid(message) => write!(f, "program is invalid: {message}"),
        }
    }
}

impl std::error::Error for CodegenError {}

/// One ISA encoder.
pub trait Encoder {
    /// ISA this encoder produces.
    fn target(&self) -> TargetIsa;

    /// Encodes one fixed-operation shader program.
    fn encode(&self, shader: &ShaderProgram) -> Result<Vec<u8>, CodegenError>;
}

/// PTX text encoder.
pub struct PtxEncoder;
/// DXBC bytecode encoder.
pub struct DxbcEncoder;
/// SPIR-V encoder for fixed-operation shader programs.
pub struct SpirvEncoder;
/// HSACO ELF encoder.
pub struct HsacoEncoder;

impl Encoder for PtxEncoder {
    fn target(&self) -> TargetIsa {
        TargetIsa::Ptx
    }

    fn encode(&self, shader: &ShaderProgram) -> Result<Vec<u8>, CodegenError> {
        emit_ptx(shader).map_err(|message| CodegenError::Encode { isa: TargetIsa::Ptx, message })
    }
}

impl Encoder for DxbcEncoder {
    fn target(&self) -> TargetIsa {
        TargetIsa::Dxbc
    }

    fn encode(&self, shader: &ShaderProgram) -> Result<Vec<u8>, CodegenError> {
        emit_dxil(shader).map_err(|message| CodegenError::Encode { isa: TargetIsa::Dxbc, message })
    }
}

impl Encoder for SpirvEncoder {
    fn target(&self) -> TargetIsa {
        TargetIsa::Spirv
    }

    fn encode(&self, shader: &ShaderProgram) -> Result<Vec<u8>, CodegenError> {
        emit_spirv(shader).map_err(|message| CodegenError::Encode { isa: TargetIsa::Spirv, message })
    }
}

impl Encoder for HsacoEncoder {
    fn target(&self) -> TargetIsa {
        TargetIsa::Hsaco
    }

    fn encode(&self, shader: &ShaderProgram) -> Result<Vec<u8>, CodegenError> {
        emit_hsaco(shader).map_err(|message| CodegenError::Encode { isa: TargetIsa::Hsaco, message })
    }
}

static PTX: PtxEncoder = PtxEncoder;
static DXBC: DxbcEncoder = DxbcEncoder;
static SPIRV: SpirvEncoder = SpirvEncoder;
static HSACO: HsacoEncoder = HsacoEncoder;

/// Returns the encoder registered for `isa`.
pub fn encoder(isa: TargetIsa) -> &'static dyn Encoder {
    match isa {
        TargetIsa::Ptx => &PTX,
        TargetIsa::Dxbc => &DXBC,
        TargetIsa::Spirv => &SPIRV,
        TargetIsa::Hsaco => &HSACO,
    }
}

/// Encodes a fixed-operation shader program for `isa`.
pub fn encode_shader(shader: &ShaderProgram, isa: TargetIsa) -> Result<EncodedModule, CodegenError> {
    let bytes = encoder(isa).encode(shader)?;
    Ok(EncodedModule { isa, bytes })
}

/// Encodes a generic UCF program for `isa`.
///
/// The program is validated and optimized once, then handed to the ISA
/// encoder. Only SPIR-V accepts the generic program shape today.
pub fn encode_program(program: &Program, entry: &str, isa: TargetIsa, options: CodegenOptions) -> Result<EncodedModule, CodegenError> {
    validate_program(program).map_err(|error| CodegenError::Invalid(error.to_string()))?;
    let optimized;
    let program = if options.optimize {
        optimized = optimize_program(program, OptimizeOptions::all());
        &optimized
    }
    else {
        program
    };
    if isa != TargetIsa::Spirv {
        return Err(CodegenError::Unsupported { isa, reason: "generic UCF programs still lack a program-level encoder for this ISA".into() });
    }
    let bytes =
        emit_program_spirv(program, entry).map_err(|error| CodegenError::Encode { isa: TargetIsa::Spirv, message: error.to_string() })?;
    Ok(EncodedModule { isa, bytes })
}
