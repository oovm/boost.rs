//! Build-time WGSL emission for every compositor operator.
//!
//! The UCF programs are the same ones the D3D12 path dispatches (see
//! [`crate::kernels`]); this module emits them through the text ABI v2 to WGSL
//! and publishes the manifest a WebGPU host needs to bind and pack them.
//!
//! The text ABI's params block starts with `count`, whereas
//! [`crate::GpuCompositor`]'s dispatch params start with `width`: that is a
//! design difference, not a defect. Hosts pack `count` from
//! [`ShaderManifest::field_offsets`] and must write `width * height` there,
//! exactly the number of pixels the dispatch covers.

use std::fmt;

use hardware_emitter::{
    ABI_VERSION, BufferElement, FieldOffset, MinSize, ParamTy, ResourceAccess, ResourceDecl, ShaderInterface, ShaderStage, SlotMap, SourceError,
    SourceTarget, UniformDecl, emit_shader_source,
};
use hardware_program::{Program, program_hash};

use crate::kernels::{DST, Op, SCRATCH, SRC, STOPS, WORKGROUP, build_program};

/// Host-declared capacity of the frame bindings (`src`, `dst`, `scratch`).
///
/// The kernels are dimension-agnostic (width and height arrive as uniforms), so
/// a build-time bundle cannot state the real frame area. The WGSL path
/// therefore declares a fixed capacity — 8 388 608 elements, 32 MiB per frame
/// buffer, e.g. 4096×2048 — and hosts must reject larger frames rather than
/// bind a smaller buffer.
pub const FRAME_ELEMENTS: u32 = 1 << 23;

/// Host-declared capacity of the gradient-stop binding.
pub const STOP_ELEMENTS: u32 = 256;

/// Uniform slots every kernel declares: `count` plus 12 `f32` parameter words.
const PARAM_SLOTS: u32 = 12;

/// One emitted kernel: the WGSL text plus the manifest that binds it.
#[derive(Debug, Clone, PartialEq)]
pub struct ShaderBundle {
    /// Variant name, e.g. `resample`.
    pub name: String,
    /// WGSL compute source.
    pub wgsl: String,
    /// Binding manifest derived from the emitter output.
    pub manifest: ShaderManifest,
}

/// Everything a WebGPU host needs to bind the kernel and pack its params.
#[derive(Debug, Clone, PartialEq)]
pub struct ShaderManifest {
    /// Frozen text ABI version the WGSL was emitted under.
    pub abi_version: u32,
    /// Workgroup shape.
    pub workgroup: [u32; 3],
    /// Logical resource id to binding slot, params first.
    pub bindings: Vec<SlotMap>,
    /// Minimum element counts per storage resource (the declared capacities).
    pub min_binding_sizes: Vec<MinSize>,
    /// Params field offsets, `count` first.
    pub field_offsets: Vec<FieldOffset>,
    /// Total params block size in bytes.
    pub param_bytes: u32,
    /// Hash of the UCF program the WGSL was emitted from.
    pub program_hash: u64,
}

/// A WGSL bundle could not be emitted.
#[derive(Debug, Clone)]
pub struct BundleError {
    /// Variant that failed.
    pub variant: String,
    /// Emitter diagnostic.
    pub message: String,
}

impl fmt::Display for BundleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "wgsl bundle {}: {}", self.variant, self.message)
    }
}

impl std::error::Error for BundleError {}

/// Emits every operator variant to WGSL plus its binding manifest.
pub fn shader_bundles() -> Result<Vec<ShaderBundle>, BundleError> {
    Op::ALL.into_iter().map(bundle).collect()
}

fn bundle(op: Op) -> Result<ShaderBundle, BundleError> {
    let program = build_program(op);
    let interface = interface(&program);
    let source = emit_shader_source(&program, &interface, SourceTarget::Wgsl)
        .map_err(|error: SourceError| BundleError { variant: op.name().to_string(), message: error.to_string() })?;
    let manifest = ShaderManifest {
        abi_version: ABI_VERSION,
        workgroup: source.workgroup_size,
        bindings: source.bindings.clone(),
        min_binding_sizes: source.min_binding_sizes.clone(),
        field_offsets: source.field_offsets.clone(),
        param_bytes: source.param_bytes,
        program_hash: program_hash(&program),
    };
    Ok(ShaderBundle { name: op.name().to_string(), wgsl: source.source, manifest })
}

fn interface(program: &Program) -> ShaderInterface {
    ShaderInterface {
        stage: ShaderStage::Compute,
        entry: program.functions[program.entry].name.clone(),
        workgroup: [WORKGROUP, 1, 1],
        abi_version: ABI_VERSION,
        program_version: program.version,
        params: (0..PARAM_SLOTS).map(|slot| UniformDecl { slot, ty: ParamTy::F32 }).collect(),
        resources: vec![
            resource(SRC, "src", ResourceAccess::ReadOnly, Some(FRAME_ELEMENTS)),
            resource(STOPS, "stops", ResourceAccess::ReadOnly, Some(STOP_ELEMENTS)),
            resource(DST, "dst", ResourceAccess::WriteOnly, Some(FRAME_ELEMENTS)),
            resource(SCRATCH, "scratch", ResourceAccess::WriteOnly, Some(FRAME_ELEMENTS)),
        ],
    }
}

fn resource(id: u64, name: &str, access: ResourceAccess, elements: Option<u32>) -> ResourceDecl {
    ResourceDecl { id: id as u32, name: name.to_string(), access, element: BufferElement::U32, elements }
}
