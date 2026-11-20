//! UCF program → WGSL / HLSL / GLSL compute shader source.
//!
//! Contract 44 text emitter. One request selects one entry function, one
//! stage and one profile; the physical layout produced here is the frozen
//! text ABI ([`ABI_VERSION`]) and is independent of the DXBC register ABI.
//! Hosts bind from the returned [`ShaderSource`] manifest, never by parsing
//! the generated text. Constructs outside the supported subset fail with a
//! [`SourceError`] carrying target, entry and IR location; a failure never
//! returns a partial manifest.

use std::collections::{BTreeMap, BTreeSet};

use hardware_program::{
    program_hash, validate_program, BinOp, CfgBlock, CfgInstruction, CfgTerminator, Expr, Function, Program, ScalarTy, Stmt, Ty, UnOp,
};

/// Frozen text-ABI version of this emitter slice.
pub const ABI_VERSION: u32 = 1;
/// Emitter identity mixed into every artifact hash.
pub const EMITTER_VERSION: &str = "ucf-text-emit/1";
/// Threads per workgroup of the first compute ABI slice, fixed at `(64, 1, 1)`.
pub const WORKGROUP_SIZE: u32 = 64;
/// Sentinel resource id used in [`ShaderSource::bindings`] for the params block.
pub const PARAM_RESOURCE: u32 = u32::MAX;

/// Text emission target selected per request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceTarget {
    /// WGSL for the WebGPU compute pipeline.
    Wgsl,
    /// HLSL `cs_5_0` for the D3D tool chain.
    Hlsl,
    /// GLSL 4.30 core compute for offline OpenGL interop.
    Glsl,
}

impl SourceTarget {
    /// Stable lower-case target name used in manifests and the capability matrix.
    pub fn name(self) -> &'static str {
        match self {
            Self::Wgsl => "wgsl",
            Self::Hlsl => "hlsl",
            Self::Glsl => "glsl",
        }
    }
}

impl std::fmt::Display for SourceTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Shader stage requested through [`ShaderInterface::stage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShaderStage {
    /// Compute dispatch with one-dimensional `GlobalId`.
    Compute,
    /// Vertex stage; the raster contract is not ready and emission fails.
    Vertex,
    /// Fragment stage; the raster contract is not ready and emission fails.
    Fragment,
}

/// Declared host type of one logical uniform slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamTy {
    /// 32-bit unsigned integer word.
    U32,
    /// 32-bit IEEE-754 float stored as its raw word bits.
    F32,
}

/// Element type of a declared storage resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferElement {
    /// One `u32` per element.
    U32,
}

/// Declared access permission of a storage resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceAccess {
    /// The program only reads this resource.
    ReadOnly,
    /// The program only writes this resource; reads are rejected even where
    /// the target language spells the binding read-write.
    WriteOnly,
}

/// One declared uniform parameter slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UniformDecl {
    /// Logical uniform slot as used by `Expr::LoadUniform`.
    pub slot: u32,
    /// Host-declared type of the slot.
    pub ty: ParamTy,
}

/// One declared storage resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceDecl {
    /// Logical resource id as used by `LoadBuffer` / `StoreBuffer`.
    pub id: u32,
    /// Target-language buffer name; must be a plain identifier.
    pub name: String,
    /// Declared access direction.
    pub access: ResourceAccess,
    /// Element type; the first ABI slice only supports [`BufferElement::U32`].
    pub element: BufferElement,
}

/// Entry declaration for one text emission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShaderInterface {
    /// Requested stage; only [`ShaderStage::Compute`] is emittable today.
    pub stage: ShaderStage,
    /// Name of the UCF entry function to emit.
    pub entry: String,
    /// Workgroup shape; must equal the frozen `(64, 1, 1)`.
    pub workgroup: [u32; 3],
    /// Text ABI version; must equal [`ABI_VERSION`].
    pub abi_version: u32,
    /// Program version; must equal `Program::version`.
    pub program_version: u32,
    /// Declared uniform parameter slots.
    pub params: Vec<UniformDecl>,
    /// Declared storage resources.
    pub resources: Vec<ResourceDecl>,
}

/// Mapping from a logical resource to its physical target slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotMap {
    /// Logical resource id, or [`PARAM_RESOURCE`] for the params block.
    pub resource: u32,
    /// Slot class: `uniform`, `storage`, `srv` or `uav`.
    pub class: &'static str,
    /// Physical register / binding index inside the class.
    pub index: u32,
}

/// Minimum host-side binding size for one storage resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MinSize {
    /// Logical resource id.
    pub resource: u32,
    /// Bytes per element.
    pub element_bytes: u32,
    /// Elements required by static indices regardless of `count`.
    pub min_elements_static: u32,
    /// The resource is indexed by `GlobalId` and needs at least `count` elements.
    pub count_dependent: bool,
}

/// Byte offset of one params-block field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldOffset {
    /// Field name: `count` or `uniform_{slot}`.
    pub name: String,
    /// Little-endian byte offset inside the params block.
    pub byte_offset: u32,
}

/// Source line back to its IR position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLocation {
    /// 1-based line in [`ShaderSource::source`].
    pub line: u32,
    /// UCF function containing the IR position.
    pub function: String,
    /// CFG block id for SSA bodies.
    pub block: Option<u32>,
    /// Statement or instruction index inside the enclosing body / block.
    pub statement: Option<u32>,
}

/// Emitted text plus the manifest a host needs to bind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShaderSource {
    /// UTF-8 shader text.
    pub source: String,
    /// Actual entry point name in the text (`main` for GLSL).
    pub entry_point: String,
    /// Stage the text was emitted for.
    pub stage: ShaderStage,
    /// Target profile: `wgsl`, `cs_5_0` or `430 core`.
    pub profile: String,
    /// Workgroup shape carried into the manifest.
    pub workgroup_size: [u32; 3],
    /// Logical resource to physical slot mapping, params first.
    pub bindings: Vec<SlotMap>,
    /// Minimum binding sizes per storage resource, sorted by resource id.
    pub min_binding_sizes: Vec<MinSize>,
    /// Total byte size of the params block.
    pub param_bytes: u32,
    /// Params-block field offsets, sorted by offset.
    pub field_offsets: Vec<FieldOffset>,
    /// Device capabilities the text relies on, in a fixed order.
    pub device_capabilities: Vec<String>,
    /// Source line to IR position mapping.
    pub source_map: Vec<SourceLocation>,
    /// Artifact identity over UCF content, entry, profile, target and versions.
    pub hash: String,
}

/// IR position attached to every [`SourceError`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrLocation {
    /// UCF function containing the offending construct.
    pub function: String,
    /// CFG block id when known.
    pub block: Option<u32>,
    /// Statement or instruction index when known.
    pub statement: Option<u32>,
    /// SSA value id when known.
    pub value: Option<u32>,
}

/// Structured emission failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// The program is not a valid UCF program or reads an undefined local.
    InvalidIr {
        /// Emission target.
        target: SourceTarget,
        /// Entry name from the interface.
        entry: String,
        /// IR position when known.
        location: Option<IrLocation>,
        /// Human-readable detail.
        message: String,
    },
    /// The IR is valid but the target or this ABI slice cannot represent it.
    CapabilityMissing {
        /// Emission target.
        target: SourceTarget,
        /// Entry name from the interface.
        entry: String,
        /// IR position when known.
        location: Option<IrLocation>,
        /// Human-readable detail.
        message: String,
    },
    /// The declared interface contradicts the program or the frozen ABI.
    AbiMismatch {
        /// Emission target.
        target: SourceTarget,
        /// Entry name from the interface.
        entry: String,
        /// IR position when known.
        location: Option<IrLocation>,
        /// Human-readable detail.
        message: String,
    },
    /// The CFG shape is outside the recoverable structured subset.
    UnrecoverableControlFlow {
        /// Emission target.
        target: SourceTarget,
        /// Entry name from the interface.
        entry: String,
        /// IR position when known.
        location: Option<IrLocation>,
        /// Human-readable detail.
        message: String,
    },
}

impl SourceError {
    /// Emission target of the failure.
    pub fn target(&self) -> SourceTarget {
        match self {
            Self::InvalidIr { target, .. }
            | Self::CapabilityMissing { target, .. }
            | Self::AbiMismatch { target, .. }
            | Self::UnrecoverableControlFlow { target, .. } => *target,
        }
    }

    /// Entry name of the failure.
    pub fn entry(&self) -> &str {
        match self {
            Self::InvalidIr { entry, .. }
            | Self::CapabilityMissing { entry, .. }
            | Self::AbiMismatch { entry, .. }
            | Self::UnrecoverableControlFlow { entry, .. } => entry,
        }
    }

    /// IR position of the failure when known.
    pub fn location(&self) -> Option<&IrLocation> {
        match self {
            Self::InvalidIr { location, .. }
            | Self::CapabilityMissing { location, .. }
            | Self::AbiMismatch { location, .. }
            | Self::UnrecoverableControlFlow { location, .. } => location.as_ref(),
        }
    }
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, target, entry, location, message) = match self {
            Self::InvalidIr { target, entry, location, message } => ("invalid IR", target, entry, location, message),
            Self::CapabilityMissing { target, entry, location, message } => ("capability missing", target, entry, location, message),
            Self::AbiMismatch { target, entry, location, message } => ("ABI mismatch", target, entry, location, message),
            Self::UnrecoverableControlFlow { target, entry, location, message } => ("unrecoverable control flow", target, entry, location, message),
        };
        write!(f, "{kind} for {target} entry `{entry}`")?;
        if let Some(location) = location {
            write!(f, " at `{}`", location.function)?;
            if let Some(block) = location.block {
                write!(f, " block {block}")?;
            }
            if let Some(statement) = location.statement {
                write!(f, " statement {statement}")?;
            }
            if let Some(value) = location.value {
                write!(f, " value {value}")?;
            }
        }
        write!(f, ": {message}")
    }
}

impl std::error::Error for SourceError {}

/// One row of the frozen capability matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityRow {
    /// Target name (`wgsl`, `hlsl`, `glsl`).
    pub target: &'static str,
    /// Feature key.
    pub feature: &'static str,
    /// Whether this emitter slice can produce the feature for the target.
    pub supported: bool,
}

/// Frozen capability matrix for the first compute ABI slice, sorted by
/// target then feature.
pub fn capability_matrix() -> Vec<CapabilityRow> {
    let mut out = Vec::new();
    for target in ["glsl", "hlsl", "wgsl"] {
        for feature in CAP_FEATURES {
            out.push(CapabilityRow { target, feature, supported: feature_supported(target, feature) });
        }
    }
    out
}

/// Whether the emitter slice can produce `feature` for `target`.
fn feature_supported(target: &str, feature: &str) -> bool {
    match feature {
        "f32-mod" => target != "glsl",
        "isfinite" => target != "wgsl",
        "cfg-back-edge"
        | "function-call"
        | "i32"
        | "f64"
        | "indirect-index-unbounded"
        | "raster-stage"
        | "u32-div"
        | "u32-mod"
        | "vec3" => false,
        _ => true,
    }
}

/// Shared feature list, sorted.
const CAP_FEATURES: &[&str] = &[
    "atan2-f32",
    "cfg-acyclic-branch",
    "cfg-back-edge",
    "f32-mod",
    "f64",
    "function-call",
    "i32",
    "indirect-index-unbounded",
    "isfinite",
    "pow-f32",
    "raster-stage",
    "scalar-bool",
    "scalar-f32",
    "scalar-u32",
    "select-lazy",
    "u32-div",
    "u32-mod",
    "uniform-u32x4-words",
    "vec3",
    "workgroup-64x1x1",
];

/// Scalar value classes the text ABI can represent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VType {
    Bool,
    F32,
    U32,
}

/// Emitted expression: statements that must run first, then the value text.
struct Emitted {
    pre: Vec<String>,
    text: String,
}

impl Emitted {
    fn plain(text: String) -> Self {
        Self { pre: Vec::new(), text }
    }
}

/// Static or dispatch-count element bound for one buffer index.
#[derive(Debug, Clone, Copy)]
enum Bound {
    Static(u64),
    Count,
}

#[derive(Debug, Clone, Copy)]
struct ResBound {
    static_max: u64,
    count_dependent: bool,
}

/// SSA index trace result cache source.
enum SsaDef {
    ConstU32(u32),
    GlobalId,
    Param(u32),
}

const LANES: [char; 4] = ['x', 'y', 'z', 'w'];

/// Emit WGSL / HLSL / GLSL compute source for one UCF entry and interface.
///
/// The result is either the full text plus manifest or a [`SourceError`];
/// failures never carry a partial manifest. See the module docs for the
/// frozen layout rules.
pub fn emit_shader_source(program: &Program, iface: &ShaderInterface, target: SourceTarget) -> Result<ShaderSource, SourceError> {
    if iface.abi_version != ABI_VERSION {
        return Err(SourceError::AbiMismatch {
            target,
            entry: iface.entry.clone(),
            location: None,
            message: format!("interface declares ABI version {}, emitter is frozen at {ABI_VERSION}", iface.abi_version),
        });
    }
    if iface.program_version != program.version {
        return Err(SourceError::AbiMismatch {
            target,
            entry: iface.entry.clone(),
            location: None,
            message: format!("interface declares program version {}, program is {}", iface.program_version, program.version),
        });
    }
    if iface.stage != ShaderStage::Compute {
        return Err(SourceError::CapabilityMissing {
            target,
            entry: iface.entry.clone(),
            location: None,
            message: "raster stage contract not ready".into(),
        });
    }
    if iface.workgroup != [WORKGROUP_SIZE, 1, 1] {
        return Err(SourceError::AbiMismatch {
            target,
            entry: iface.entry.clone(),
            location: None,
            message: format!("workgroup {:?} does not match the frozen ABI shape [{WORKGROUP_SIZE}, 1, 1]", iface.workgroup),
        });
    }
    validate_program(program).map_err(|error| SourceError::InvalidIr {
        target,
        entry: iface.entry.clone(),
        location: None,
        message: format!("invalid UCF program: {error}"),
    })?;
    let function = program.functions.iter().find(|candidate| candidate.name == iface.entry).ok_or_else(|| SourceError::AbiMismatch {
        target,
        entry: iface.entry.clone(),
        location: None,
        message: format!("entry function `{}` is not present in the program", iface.entry),
    })?;
    if !function.params.is_empty() {
        return Err(SourceError::CapabilityMissing {
            target,
            entry: iface.entry.clone(),
            location: None,
            message: "function parameters are outside the text ABI subset".into(),
        });
    }
    if !is_identifier(&function.name) && target != SourceTarget::Glsl {
        return Err(SourceError::AbiMismatch {
            target,
            entry: iface.entry.clone(),
            location: None,
            message: format!("entry name `{}` is not a target identifier", function.name),
        });
    }

    let mut uniform_slots = BTreeMap::new();
    for decl in &iface.params {
        if uniform_slots.insert(decl.slot, decl.ty).is_some() {
            return Err(SourceError::AbiMismatch {
                target,
                entry: iface.entry.clone(),
                location: None,
                message: format!("uniform slot {} is declared twice", decl.slot),
            });
        }
    }
    let mut resources = BTreeMap::new();
    let mut names = BTreeSet::new();
    for decl in &iface.resources {
        if !is_identifier(&decl.name) {
            return Err(SourceError::AbiMismatch {
                target,
                entry: iface.entry.clone(),
                location: None,
                message: format!("resource name `{}` is not a target identifier", decl.name),
            });
        }
        if !names.insert(decl.name.clone()) {
            return Err(SourceError::AbiMismatch {
                target,
                entry: iface.entry.clone(),
                location: None,
                message: format!("resource name `{}` is declared twice", decl.name),
            });
        }
        if decl.element != BufferElement::U32 {
            return Err(SourceError::AbiMismatch {
                target,
                entry: iface.entry.clone(),
                location: None,
                message: format!("resource {} element is not u32", decl.id),
            });
        }
        if resources.insert(decl.id, decl).is_some() {
            return Err(SourceError::AbiMismatch {
                target,
                entry: iface.entry.clone(),
                location: None,
                message: format!("resource id {} is declared twice", decl.id),
            });
        }
    }

    let words = uniform_slots.keys().next_back().map(|slot| slot.saturating_add(2).div_ceil(4)).unwrap_or(1).max(1);
    let mut emitter = Emitter {
        target,
        iface,
        entry: &function.name,
        words,
        uniform_slots,
        resources,
        out: Vec::new(),
        indent: 0,
        source_map: Vec::new(),
        block: None,
        statement: None,
        value: None,
        locals: BTreeMap::new(),
        assigned: BTreeSet::new(),
        iter_limits: BTreeMap::new(),
        temp: 0,
        loop_counter: 0,
        loop_depth: 0,
        res_bounds: BTreeMap::new(),
        ssa_types: BTreeMap::new(),
        ssa_defs: BTreeMap::new(),
        incoming: BTreeMap::new(),
    };
    emitter.emit_declarations()?;
    emitter.emit_guard();
    match function.body.as_slice() {
        [Stmt::Cfg { entry, blocks }] => emitter.emit_cfg(*entry, blocks)?,
        body => {
            if body.iter().any(|stmt| matches!(stmt, Stmt::Cfg { .. })) {
                return Err(emitter.capability("CFG statements must be the sole entry body statement".into()));
            }
            emitter.collect_locals(function)?;
            emitter.declare_locals();
            // A `return` as the final body statement is implicit at the end of
            // the void entry in all three targets: validate its value
            // expression for capability and type errors but drop the text.
            let (body, trailing) = match body.split_last() {
                Some((Stmt::Return(value), head)) => (head, Some(value)),
                _ => (body, None),
            };
            emitter.walk(body)?;
            if let Some(value) = trailing {
                emitter.expr(value)?;
            }
        }
    }
    emitter.indent = 0;
    emitter.push("}");

    let source = emitter.out.join("\n") + "\n";
    let profile = match target {
        SourceTarget::Wgsl => "wgsl",
        SourceTarget::Hlsl => "cs_5_0",
        SourceTarget::Glsl => "430 core",
    };
    let entry_point = match target {
        SourceTarget::Glsl => "main".to_string(),
        _ => function.name.clone(),
    };

    let mut bindings = vec![SlotMap { resource: PARAM_RESOURCE, class: "uniform", index: 0 }];
    match target {
        SourceTarget::Wgsl | SourceTarget::Glsl => {
            for (rank, id) in emitter.resources.keys().enumerate() {
                bindings.push(SlotMap { resource: *id, class: "storage", index: rank as u32 + 1 });
            }
        }
        SourceTarget::Hlsl => {
            let mut t = 0u32;
            let mut u = 0u32;
            for (id, decl) in &emitter.resources {
                match decl.access {
                    ResourceAccess::ReadOnly => {
                        bindings.push(SlotMap { resource: *id, class: "srv", index: t });
                        t += 1;
                    }
                    ResourceAccess::WriteOnly => {
                        bindings.push(SlotMap { resource: *id, class: "uav", index: u });
                        u += 1;
                    }
                }
            }
        }
    }

    let min_binding_sizes = emitter
        .resources
        .iter()
        .map(|(id, _)| {
            let bound = emitter.res_bounds.get(id).copied().unwrap_or(ResBound { static_max: 0, count_dependent: false });
            MinSize {
                resource: *id,
                element_bytes: 4,
                min_elements_static: bound.static_max.min(u32::MAX as u64) as u32,
                count_dependent: bound.count_dependent,
            }
        })
        .collect();

    let mut field_offsets = vec![FieldOffset { name: "count".into(), byte_offset: 0 }];
    for slot in emitter.uniform_slots.keys() {
        field_offsets.push(FieldOffset { name: format!("uniform_{slot}"), byte_offset: (slot + 1) * 4 });
    }
    field_offsets.sort_by_key(|field| field.byte_offset);

    let device_capabilities: &[&str] = match target {
        SourceTarget::Wgsl => &["compute-stage", "storage-buffer-read", "storage-buffer-read-write", "uniform-buffer-vec4-groups", "workgroup-size-64"],
        SourceTarget::Hlsl => &["cs_5_0", "cbuffer-u32x4", "rw-structured-buffer-uint", "structured-buffer-uint", "numthreads-64"],
        SourceTarget::Glsl => &["glsl-430-core", "std140-u32x4-params", "std430-ssbo-uint", "local-size-x-64"],
    };
    let device_capabilities = device_capabilities.iter().map(|cap| (*cap).to_string()).collect();

    let hash = artifact_hash(program, &entry_point, profile, target);
    Ok(ShaderSource {
        source,
        entry_point,
        stage: ShaderStage::Compute,
        profile: profile.into(),
        workgroup_size: iface.workgroup,
        bindings,
        min_binding_sizes,
        param_bytes: words * 16,
        field_offsets,
        device_capabilities,
        source_map: emitter.source_map,
        hash,
    })
}

/// Emitter state for one request.
struct Emitter<'a> {
    target: SourceTarget,
    iface: &'a ShaderInterface,
    entry: &'a str,
    words: u32,
    uniform_slots: BTreeMap<u32, ParamTy>,
    resources: BTreeMap<u32, &'a ResourceDecl>,
    out: Vec<String>,
    indent: usize,
    source_map: Vec<SourceLocation>,
    block: Option<u32>,
    statement: Option<u32>,
    value: Option<u32>,
    locals: BTreeMap<u32, VType>,
    assigned: BTreeSet<u32>,
    iter_limits: BTreeMap<u32, u32>,
    temp: u32,
    loop_counter: u32,
    loop_depth: usize,
    res_bounds: BTreeMap<u32, ResBound>,
    ssa_types: BTreeMap<u32, VType>,
    ssa_defs: BTreeMap<u32, SsaDef>,
    incoming: BTreeMap<u32, Vec<Vec<u32>>>,
}

impl<'a> Emitter<'a> {
    fn push(&mut self, text: &str) -> u32 {
        self.out.push(format!("{}{text}", "    ".repeat(self.indent)));
        self.out.len() as u32
    }

    fn push_pre(&mut self, pre: &[String]) {
        for line in pre {
            self.push(line);
        }
    }

    fn indent_lines(lines: &[String]) -> Vec<String> {
        lines.iter().map(|line| if line.is_empty() { line.clone() } else { format!("    {line}") }).collect()
    }

    fn location(&self) -> Option<IrLocation> {
        if self.block.is_none() && self.statement.is_none() && self.value.is_none() {
            return None;
        }
        Some(IrLocation { function: self.entry.into(), block: self.block, statement: self.statement, value: self.value })
    }

    fn invalid(&self, message: String) -> SourceError {
        SourceError::InvalidIr { target: self.target, entry: self.iface.entry.clone(), location: self.location(), message }
    }

    fn capability(&self, message: String) -> SourceError {
        SourceError::CapabilityMissing { target: self.target, entry: self.iface.entry.clone(), location: self.location(), message }
    }

    fn abi(&self, message: String) -> SourceError {
        SourceError::AbiMismatch { target: self.target, entry: self.iface.entry.clone(), location: self.location(), message }
    }

    fn unrecoverable(&self, message: String) -> SourceError {
        SourceError::UnrecoverableControlFlow { target: self.target, entry: self.iface.entry.clone(), location: self.location(), message }
    }

    fn scalar_ty(&self, ty: &Ty) -> Result<VType, SourceError> {
        match ty {
            Ty::Scalar(ScalarTy::Bool) => Ok(VType::Bool),
            Ty::Scalar(ScalarTy::F32) => Ok(VType::F32),
            Ty::Scalar(ScalarTy::U32) => Ok(VType::U32),
            other => Err(self.capability(format!("type {other:?} is outside the text ABI subset"))),
        }
    }

    fn ty_text(&self, ty: VType) -> &'static str {
        match (self.target, ty) {
            (SourceTarget::Wgsl, VType::U32) => "u32",
            (SourceTarget::Wgsl, VType::F32) => "f32",
            (SourceTarget::Wgsl, VType::Bool) => "bool",
            (_, VType::U32) => "uint",
            (_, VType::F32) => "float",
            (_, VType::Bool) => "bool",
        }
    }

    fn default_text(&self, ty: VType) -> String {
        match ty {
            VType::U32 => "0u".into(),
            VType::F32 => self.f32_text(0.0),
            VType::Bool => "false".into(),
        }
    }

    fn decl_line(&self, name: &str, ty: VType, init: Option<&str>) -> String {
        match self.target {
            SourceTarget::Wgsl => match init {
                Some(init) => format!("var {name}: {} = {init};", self.ty_text(ty)),
                None => format!("var {name}: {};", self.ty_text(ty)),
            },
            _ => match init {
                Some(init) => format!("{} {name} = {init};", self.ty_text(ty)),
                None => format!("{} {name};", self.ty_text(ty)),
            },
        }
    }

    fn f32_text(&self, value: f32) -> String {
        let bits = value.to_bits();
        match self.target {
            SourceTarget::Wgsl => format!("bitcast<f32>(0x{bits:08x}u)"),
            SourceTarget::Hlsl => format!("asfloat(0x{bits:08x}u)"),
            SourceTarget::Glsl => format!("uintBitsToFloat(0x{bits:08x}u)"),
        }
    }

    fn id_text(&self) -> &'static str {
        match self.target {
            SourceTarget::Wgsl | SourceTarget::Hlsl => "id.x",
            SourceTarget::Glsl => "id",
        }
    }

    fn params_word(&self, word: u32) -> String {
        let lane = LANES[(word % 4) as usize];
        match self.target {
            SourceTarget::Wgsl => {
                if self.words == 1 {
                    format!("params.words.{lane}")
                } else {
                    format!("params.words[{}].{lane}", word / 4)
                }
            }
            _ => {
                if self.words == 1 {
                    format!("words.{lane}")
                } else {
                    format!("words[{}].{lane}", word / 4)
                }
            }
        }
    }

    fn uniform_text(&self, slot: u32, ty: ParamTy) -> String {
        let word = self.params_word(slot + 1);
        match ty {
            ParamTy::U32 => word,
            ParamTy::F32 => match self.target {
                SourceTarget::Wgsl => format!("bitcast<f32>({word})"),
                SourceTarget::Hlsl => format!("asfloat({word})"),
                SourceTarget::Glsl => format!("uintBitsToFloat({word})"),
            },
        }
    }

    fn emit_declarations(&mut self) -> Result<(), SourceError> {
        let declared: Vec<&ResourceDecl> = self.resources.values().map(|decl| *decl).collect();
        match self.target {
            SourceTarget::Wgsl => {
                let words = if self.words == 1 { "vec4<u32>".into() } else { format!("array<vec4<u32>, {}>", self.words) };
                self.push(&format!("struct Params {{ words: {words}, }}"));
                self.push("@group(0) @binding(0) var<uniform> params: Params;");
                for (rank, decl) in declared.iter().enumerate() {
                    let access = match decl.access {
                        ResourceAccess::ReadOnly => "read",
                        ResourceAccess::WriteOnly => "read_write",
                    };
                    self.push(&format!("@group(0) @binding({}) var<storage, {access}> {}: array<u32>;", rank + 1, decl.name));
                }
                self.push("");
                self.push("@compute @workgroup_size(64)");
                self.push(&format!("fn {}(@builtin(global_invocation_id) id: vec3<u32>) {{", self.entry));
            }
            SourceTarget::Hlsl => {
                let words = if self.words == 1 { "uint4 words;".into() } else { format!("uint4 words[{}];", self.words) };
                self.push(&format!("cbuffer Params : register(b0) {{ {words} }};"));
                let mut t = 0u32;
                let mut u = 0u32;
                for decl in &declared {
                    match decl.access {
                        ResourceAccess::ReadOnly => {
                            self.push(&format!("StructuredBuffer<uint> {} : register(t{t});", decl.name));
                            t += 1;
                        }
                        ResourceAccess::WriteOnly => {
                            self.push(&format!("RWStructuredBuffer<uint> {} : register(u{u});", decl.name));
                            u += 1;
                        }
                    }
                }
                self.push("");
                self.push("[numthreads(64, 1, 1)]");
                self.push("void main(uint3 id : SV_DispatchThreadID) {");
            }
            SourceTarget::Glsl => {
                self.push("#version 430 core");
                self.push("");
                let words = if self.words == 1 { "uvec4 words;".into() } else { format!("uvec4 words[{}];", self.words) };
                self.push(&format!("layout(std140, binding=0) uniform Params {{ {words} }};"));
                for (rank, decl) in declared.iter().enumerate() {
                    let access = match decl.access {
                        ResourceAccess::ReadOnly => "readonly",
                        ResourceAccess::WriteOnly => "writeonly",
                    };
                    self.push(&format!(
                        "layout(std430, binding={}) {access} buffer {} {{ uint {}[]; }};",
                        rank + 1,
                        capitalize(&decl.name),
                        decl.name
                    ));
                }
                self.push("");
                self.push("layout(local_size_x = 64) in;");
                self.push("");
                self.push("void main() {");
                self.indent = 1;
                self.push("uint id = gl_GlobalInvocationID.x;");
            }
        }
        self.indent = 1;
        Ok(())
    }

    fn emit_guard(&mut self) {
        let index = self.id_text();
        let count = self.params_word(0);
        self.push(&format!("if ({index} >= {count}) {{ return; }}"));
    }

    fn record_bound(&mut self, resource: u32, bound: Bound) {
        let entry = self.res_bounds.entry(resource).or_insert(ResBound { static_max: 0, count_dependent: false });
        match bound {
            Bound::Static(n) => entry.static_max = entry.static_max.max(n),
            Bound::Count => entry.count_dependent = true,
        }
    }

    fn trace_structured_index(&self, expr: &Expr) -> Result<Bound, SourceError> {
        match expr {
            Expr::ConstU32(value) => match u64::from(*value).checked_add(1) {
                Some(bound) => Ok(Bound::Static(bound)),
                None => Err(self.capability("constant buffer index overflows the element domain".into())),
            },
            Expr::GlobalId => Ok(Bound::Count),
            Expr::LoadLocal(id) => match self.iter_limits.get(id) {
                Some(limit) => Ok(Bound::Static(u64::from(*limit))),
                None => Err(self.capability(format!("indirect index through local {id} is outside the bounded subset"))),
            },
            _ => Err(self.capability("indirect buffer index is outside the bounded subset".into())),
        }
    }

    fn trace_ssa_index(&self, value: u32, visiting: &mut BTreeSet<u32>) -> Option<Bound> {
        if !visiting.insert(value) {
            return None;
        }
        let bound = match self.ssa_defs.get(&value)? {
            SsaDef::ConstU32(const_value) => u64::from(*const_value).checked_add(1).map(Bound::Static)?,
            SsaDef::GlobalId => Bound::Count,
            SsaDef::Param(block) => {
                let edges = self.incoming.get(block)?;
                let mut best: Option<u64> = None;
                for args in edges {
                    for arg in args {
                        if arg == &value {
                            return None;
                        }
                        match self.trace_ssa_index(*arg, visiting)? {
                            Bound::Static(n) => best = Some(best.map_or(n, |current: u64| current.min(n))),
                            Bound::Count => {}
                        }
                    }
                }
                match best {
                    Some(n) => Bound::Static(n),
                    None => Bound::Count,
                }
            }
        };
        Some(bound)
    }

    fn ssa_index_bound(&self, value: u32) -> Result<Bound, SourceError> {
        let mut visiting = BTreeSet::new();
        self.trace_ssa_index(value, &mut visiting)
            .ok_or_else(|| self.capability(format!("indirect buffer index through SSA value {value} is outside the bounded subset")))
    }

    fn declare_local(&mut self, id: u32, ty: VType) {
        let default = self.default_text(ty);
        self.push(&self.decl_line(&format!("v{id}"), ty, Some(&default)));
    }

    fn collect_locals(&mut self, function: &Function) -> Result<(), SourceError> {
        for (id, ty) in &function.locals {
            let vt = self.scalar_ty(ty)?;
            self.locals.insert(*id, vt);
        }
        self.collect_locals_stmts(&function.body)?;
        Ok(())
    }

    fn collect_locals_stmts(&mut self, body: &[Stmt]) -> Result<(), SourceError> {
        for stmt in body {
            match stmt {
                Stmt::Let { id, ty, .. } => {
                    let vt = self.scalar_ty(ty)?;
                    if let Some(existing) = self.locals.get(id).copied() {
                        if existing != vt {
                            return Err(self.abi(format!("local {id} is declared with conflicting types")));
                        }
                    }
                    self.locals.insert(*id, vt);
                }
                Stmt::StoreLocal { id, .. } => {
                    if !self.locals.contains_key(id) {
                        return Err(self.invalid(format!("store to undeclared local {id}")));
                    }
                }
                Stmt::StoreBuffer { .. } | Stmt::Return(_) | Stmt::Expr(_) | Stmt::Break | Stmt::Continue => {}
                Stmt::For { iter, body, .. } => {
                    match self.locals.get(iter).copied() {
                        Some(VType::U32) => {}
                        Some(_) => return Err(self.abi(format!("for iterator local {iter} is not u32"))),
                        None => {
                            self.locals.insert(*iter, VType::U32);
                        }
                    }
                    self.collect_locals_stmts(body)?;
                }
                Stmt::If { then_body, else_body, .. } => {
                    self.collect_locals_stmts(then_body)?;
                    self.collect_locals_stmts(else_body)?;
                }
                Stmt::While { body, .. } => self.collect_locals_stmts(body)?,
                Stmt::Cfg { .. } => return Err(self.capability("nested CFG statement is outside the text ABI subset".into())),
            }
        }
        Ok(())
    }

    fn declare_locals(&mut self) {
        let ids: Vec<(u32, VType)> = self.locals.iter().map(|(id, ty)| (*id, *ty)).collect();
        for (id, ty) in ids {
            self.declare_local(id, ty);
        }
    }

    fn expr(&mut self, expr: &Expr) -> Result<(Emitted, VType), SourceError> {
        match expr {
            Expr::ConstF32(value) => Ok((Emitted::plain(self.f32_text(*value)), VType::F32)),
            Expr::ConstF64(_) => Err(self.capability("f64 constants are outside the text ABI subset".into())),
            Expr::ConstBool(value) => Ok((Emitted::plain(value.to_string()), VType::Bool)),
            Expr::ConstU32(value) => Ok((Emitted::plain(format!("{value}u")), VType::U32)),
            Expr::LoadLocal(id) => {
                let ty = *self.locals.get(id).ok_or_else(|| self.invalid(format!("unknown local {id}")))?;
                if !self.assigned.contains(id) {
                    return Err(self.invalid(format!("local {id} is read before it is definitely assigned")));
                }
                Ok((Emitted::plain(format!("v{id}")), ty))
            }
            Expr::LoadUniform(slot) => {
                let ty = *self.uniform_slots.get(slot).ok_or_else(|| self.abi(format!("uniform slot {slot} is not declared")))?;
                let vt = match ty {
                    ParamTy::U32 => VType::U32,
                    ParamTy::F32 => VType::F32,
                };
                Ok((Emitted::plain(self.uniform_text(*slot, ty)), vt))
            }
            Expr::LoadBuffer { resource, index } => {
                let decl = *self.resources.get(resource).ok_or_else(|| self.abi(format!("resource {resource} is not declared")))?;
                if decl.access != ResourceAccess::ReadOnly {
                    return Err(self.abi(format!("resource {resource} is declared write-only but is read")));
                }
                let (emitted, ty) = self.expr(index)?;
                if ty != VType::U32 {
                    return Err(self.abi("buffer index must be u32".into()));
                }
                let bound = self.trace_structured_index(index)?;
                self.record_bound(*resource, bound);
                Ok((Emitted { pre: emitted.pre, text: format!("{}[{}]", decl.name, emitted.text) }, VType::U32))
            }
            Expr::GlobalId => Ok((Emitted::plain(self.id_text().into()), VType::U32)),
            Expr::BinOp { op, lhs, rhs } => {
                let (left, lty) = self.expr(lhs)?;
                let (right, rty) = self.expr(rhs)?;
                let (out_ty, symbol) = self.binop_result(*op, lty, rty)?;
                let text = if matches!(op, BinOp::Min | BinOp::Max | BinOp::Mod | BinOp::Pow | BinOp::Atan2) {
                    self.call_text(symbol, &[left.text, right.text])
                } else {
                    format!("({} {} {})", left.text, symbol, right.text)
                };
                let mut pre = left.pre;
                pre.extend(right.pre);
                Ok((Emitted { pre, text }, out_ty))
            }
            Expr::UnOp { op, arg } => {
                let (inner, ty) = self.expr(arg)?;
                let (out_ty, text) = self.unop_result(*op, ty, &inner.text)?;
                Ok((Emitted { pre: inner.pre, text }, out_ty))
            }
            Expr::Select { cond, then_, else_ } => {
                let (cond_emitted, cond_ty) = self.expr(cond)?;
                if cond_ty != VType::Bool {
                    return Err(self.abi("select condition must be bool".into()));
                }
                let (then_emitted, then_ty) = self.expr(then_)?;
                let (else_emitted, else_ty) = self.expr(else_)?;
                if then_ty != else_ty {
                    return Err(self.abi(format!("select branches disagree: {then_ty:?} vs {else_ty:?}")));
                }
                let name = self.next_temp();
                let mut pre = cond_emitted.pre;
                pre.push(self.decl_line(&name, then_ty, None));
                pre.push(format!("if ({}) {{", cond_emitted.text));
                pre.extend(Self::indent_lines(&then_emitted.pre));
                pre.push(format!("    {name} = {};", then_emitted.text));
                pre.push("}".into());
                pre.push("else {".into());
                pre.extend(Self::indent_lines(&else_emitted.pre));
                pre.push(format!("    {name} = {};", else_emitted.text));
                pre.push("}".into());
                Ok((Emitted { pre, text: name }, then_ty))
            }
            Expr::Vec3Compose { .. } => Err(self.capability("vec3 values are outside the text ABI subset".into())),
            Expr::Call { .. } => Err(self.capability("function calls are outside the text ABI subset".into())),
        }
    }

    fn next_temp(&mut self) -> String {
        let name = format!("t{}", self.temp);
        self.temp += 1;
        name
    }

    fn same_numeric(&self, lty: VType, rty: VType) -> Result<VType, SourceError> {
        match (lty, rty) {
            (VType::U32, VType::U32) | (VType::F32, VType::F32) => Ok(lty),
            (left, right) => Err(self.abi(format!("operand type mismatch: {left:?} vs {right:?}"))),
        }
    }

    fn binop_result(&self, op: BinOp, lty: VType, rty: VType) -> Result<(VType, &'static str), SourceError> {
        match op {
            BinOp::Add => Ok((self.same_numeric(lty, rty)?, "+")),
            BinOp::Sub => Ok((self.same_numeric(lty, rty)?, "-")),
            BinOp::Mul => Ok((self.same_numeric(lty, rty)?, "*")),
            BinOp::Div => {
                if lty == VType::U32 && rty == VType::U32 {
                    return Err(self.capability("u32 division semantics are not frozen".into()));
                }
                Ok((self.same_numeric(lty, rty)?, "/"))
            }
            BinOp::Min => Ok((self.same_numeric(lty, rty)?, "min")),
            BinOp::Max => Ok((self.same_numeric(lty, rty)?, "max")),
            BinOp::Mod => {
                if lty == VType::U32 || rty == VType::U32 {
                    return Err(self.capability("u32 modulo semantics are not frozen".into()));
                }
                let ty = self.same_numeric(lty, rty)?;
                if self.target == SourceTarget::Glsl {
                    return Err(self.capability("GLSL mod() is floor-mod and does not match the CPU f32 remainder".into()));
                }
                Ok((ty, "mod"))
            }
            BinOp::Pow => {
                let ty = self.same_numeric(lty, rty)?;
                if ty != VType::F32 {
                    return Err(self.capability("pow requires f32 operands".into()));
                }
                Ok((ty, "pow"))
            }
            BinOp::Dot => Err(self.capability("vec3 dot is outside the text ABI subset".into())),
            BinOp::Atan2 => {
                if lty == VType::F32 && rty == VType::F32 {
                    Ok((VType::F32, "atan2"))
                } else {
                    Err(self.capability("atan2 requires f32 operands".into()))
                }
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let symbol = match op {
                    BinOp::Lt => "<",
                    BinOp::Le => "<=",
                    BinOp::Gt => ">",
                    _ => ">=",
                };
                self.same_numeric(lty, rty)?;
                Ok((VType::Bool, symbol))
            }
            BinOp::Eq | BinOp::Ne => {
                if lty != rty {
                    return Err(self.abi(format!("operand type mismatch: {lty:?} vs {rty:?}")));
                }
                Ok((VType::Bool, if op == BinOp::Eq { "==" } else { "!=" }))
            }
            BinOp::And | BinOp::Or | BinOp::Shl | BinOp::Shr => {
                Err(self.capability("integer bitwise and shift operations are outside the text ABI subset".into()))
            }
        }
    }

    fn call_text(&self, name: &str, args: &[String]) -> String {
        format!("{name}({})", args.join(", "))
    }

    fn unop_result(&self, op: UnOp, ty: VType, text: &str) -> Result<(VType, String), SourceError> {
        match op {
            UnOp::Neg => match ty {
                VType::F32 => Ok((VType::F32, format!("(-{text})"))),
                VType::U32 => Err(self.capability("u32 negation semantics are not frozen".into())),
                VType::Bool => Err(self.abi("neg requires a numeric operand".into())),
            },
            UnOp::Sqrt | UnOp::Sin | UnOp::Cos | UnOp::Atan | UnOp::Abs => {
                if ty != VType::F32 {
                    return Err(self.capability(format!("{op:?} requires an f32 operand")));
                }
                let name = match op {
                    UnOp::Sqrt => "sqrt",
                    UnOp::Sin => "sin",
                    UnOp::Cos => "cos",
                    UnOp::Atan => "atan",
                    _ => "abs",
                };
                Ok((VType::F32, self.call_text(name, &[text.into()])))
            }
            UnOp::Ln => {
                if ty != VType::F32 {
                    return Err(self.capability("ln requires an f32 operand".into()));
                }
                Ok((VType::F32, self.call_text("log", &[text.into()])))
            }
            UnOp::IsFinite => {
                if self.target == SourceTarget::Wgsl {
                    return Err(self.capability("WGSL has no isfinite in this ABI slice".into()));
                }
                if ty != VType::F32 {
                    return Err(self.capability("isfinite requires an f32 operand".into()));
                }
                Ok((VType::Bool, self.call_text("isfinite", &[text.into()])))
            }
            UnOp::Length | UnOp::Normalize | UnOp::Vec3Comp(_) => {
                Err(self.capability("vec3 operations are outside the text ABI subset".into()))
            }
            UnOp::Bitcast | UnOp::U32ToF32 | UnOp::F32ToU32(_) => {
                Err(self.capability("integer and float conversions are outside the text ABI subset".into()))
            }
        }
    }

    fn walk(&mut self, body: &[Stmt]) -> Result<bool, SourceError> {
        let mut fallthrough = true;
        for (index, stmt) in body.iter().enumerate() {
            self.statement = Some(index as u32);
            fallthrough = self.stmt(stmt)?;
            if !fallthrough {
                break;
            }
        }
        Ok(fallthrough)
    }

    fn stmt(&mut self, stmt: &Stmt) -> Result<bool, SourceError> {
        match stmt {
            Stmt::Let { id, ty, init } => {
                let declared = self.scalar_ty(ty)?;
                let start = self.out.len() + 1;
                let (emitted, actual) = self.expr(init)?;
                if actual != declared {
                    return Err(self.abi(format!("local {id} is declared {declared:?} but initialised with {actual:?}")));
                }
                self.push_pre(&emitted.pre);
                self.push(&format!("v{id} = {};", emitted.text));
                self.locals.insert(*id, declared);
                self.assigned.insert(*id);
                self.map_from(start);
                Ok(true)
            }
            Stmt::StoreLocal { id, value } => {
                let declared = *self.locals.get(id).ok_or_else(|| self.invalid(format!("store to undeclared local {id}")))?;
                let start = self.out.len() + 1;
                let (emitted, actual) = self.expr(value)?;
                if actual != declared {
                    return Err(self.abi(format!("store to local {id}: declared {declared:?}, value {actual:?}")));
                }
                self.push_pre(&emitted.pre);
                self.push(&format!("v{id} = {};", emitted.text));
                self.assigned.insert(*id);
                self.map_from(start);
                Ok(true)
            }
            Stmt::StoreBuffer { resource, index, value } => {
                let decl = *self.resources.get(resource).ok_or_else(|| self.abi(format!("resource {resource} is not declared")))?;
                if decl.access != ResourceAccess::WriteOnly {
                    return Err(self.abi(format!("resource {resource} is declared read-only but is written")));
                }
                let start = self.out.len() + 1;
                let (index_emitted, index_ty) = self.expr(index)?;
                if index_ty != VType::U32 {
                    return Err(self.abi("buffer index must be u32".into()));
                }
                let bound = self.trace_structured_index(index)?;
                let (value_emitted, value_ty) = self.expr(value)?;
                if value_ty != VType::U32 {
                    return Err(self.abi(format!("illegal mixed type: resource {resource} elements are u32 but the stored value is {value_ty:?}")));
                }
                self.record_bound(*resource, bound);
                self.push_pre(&index_emitted.pre);
                self.push_pre(&value_emitted.pre);
                self.push(&format!("{}[{}] = {};", decl.name, index_emitted.text, value_emitted.text));
                self.map_from(start);
                Ok(true)
            }
            Stmt::If { cond, then_body, else_body } => {
                let (cond_emitted, cond_ty) = self.expr(cond)?;
                if cond_ty != VType::Bool {
                    return Err(self.abi("if condition must be bool".into()));
                }
                let start = self.out.len() + 1;
                self.push_pre(&cond_emitted.pre);
                self.push(&format!("if ({}) {{", cond_emitted.text));
                self.indent += 1;
                let entry_assigned = self.assigned.clone();
                let then_fallthrough = self.walk(then_body)?;
                let then_assigned = std::mem::take(&mut self.assigned);
                self.indent -= 1;
                if else_body.is_empty() {
                    self.push("}");
                    self.assigned = if then_fallthrough { then_assigned.intersection(&entry_assigned).copied().collect() } else { entry_assigned };
                } else {
                    self.push("} else {");
                    self.indent += 1;
                    self.assigned = entry_assigned.clone();
                    let else_fallthrough = self.walk(else_body)?;
                    let else_assigned = std::mem::take(&mut self.assigned);
                    self.indent -= 1;
                    self.push("}");
                    self.assigned = match (then_fallthrough, else_fallthrough) {
                        (true, true) => then_assigned.intersection(&else_assigned).copied().collect(),
                        (true, false) => then_assigned,
                        (false, true) => else_assigned,
                        (false, false) => entry_assigned,
                    };
                }
                self.map_from(start);
                Ok(true)
            }
            Stmt::For { iter, limit, body } => {
                let start = self.out.len() + 1;
                let loop_var = self.next_loop_var();
                let header = match self.target {
                    SourceTarget::Wgsl => format!("for (var {loop_var}: u32 = 0u; {loop_var} < {limit}u; {loop_var} = {loop_var} + 1u) {{"),
                    _ => format!("for (uint {loop_var} = 0u; {loop_var} < {limit}u; {loop_var} = {loop_var} + 1u) {{"),
                };
                self.push(&header);
                self.indent += 1;
                self.push(&format!("v{iter} = {loop_var};"));
                let entry_assigned = self.assigned.clone();
                self.assigned.insert(*iter);
                self.iter_limits.insert(*iter, *limit);
                self.loop_depth += 1;
                self.walk(body)?;
                self.loop_depth -= 1;
                self.assigned = entry_assigned;
                self.indent -= 1;
                self.push("}");
                self.map_from(start);
                Ok(true)
            }
            Stmt::While { cond, limit, body } => {
                let (cond_emitted, cond_ty) = self.expr(cond)?;
                if cond_ty != VType::Bool {
                    return Err(self.abi("while condition must be bool".into()));
                }
                if !cond_emitted.pre.is_empty() {
                    return Err(self.capability("statement-level select in a while condition is outside the text ABI subset".into()));
                }
                let start = self.out.len() + 1;
                let guard = format!("ucf_g{}", self.loop_counter);
                self.loop_counter += 1;
                match self.target {
                    SourceTarget::Wgsl => self.push(&format!("var {guard}: u32 = 0u;")),
                    _ => self.push(&format!("uint {guard} = 0u;")),
                };
                self.push(&format!("while ({guard} < {limit}u && {}) {{", cond_emitted.text));
                self.indent += 1;
                self.push(&format!("{guard} = {guard} + 1u;"));
                let entry_assigned = self.assigned.clone();
                self.loop_depth += 1;
                self.walk(body)?;
                self.loop_depth -= 1;
                self.assigned = entry_assigned;
                self.indent -= 1;
                self.push("}");
                self.map_from(start);
                Ok(true)
            }
            Stmt::Break => {
                if self.loop_depth == 0 {
                    return Err(self.invalid("break outside a loop".into()));
                }
                self.push("break;");
                Ok(false)
            }
            Stmt::Continue => {
                if self.loop_depth == 0 {
                    return Err(self.invalid("continue outside a loop".into()));
                }
                self.push("continue;");
                Ok(false)
            }
            Stmt::Return(expr) => {
                // Pure return values are dropped: the compute entry is void,
                // outputs go through explicit storage writes, and calls (the
                // only observable side effect) are rejected outright.
                self.expr(expr)?;
                self.push("return;");
                Ok(false)
            }
            Stmt::Expr(expr) => {
                self.expr(expr)?;
                Ok(true)
            }
            Stmt::Cfg { .. } => Err(self.capability("nested CFG statement is outside the text ABI subset".into())),
        }
    }

    fn next_loop_var(&mut self) -> String {
        let name = format!("ucf_i{}", self.loop_counter);
        self.loop_counter += 1;
        name
    }

    fn map_from(&mut self, start: usize) {
        let line = start as u32;
        if line <= self.out.len() as u32 {
            self.source_map.push(SourceLocation {
                line,
                function: self.entry.into(),
                block: self.block,
                statement: self.statement,
            });
        }
    }

    fn ssa_text(&self, value: u32) -> String {
        format!("s{value}")
    }

    fn ssa_type(&self, value: u32) -> Result<VType, SourceError> {
        self.ssa_types.get(&value).copied().ok_or_else(|| self.invalid(format!("SSA value {value} is used before it is defined")))
    }

    fn declare_ssa(&mut self, result: u32, ty: VType, text: String) {
        self.push(&self.decl_line(&self.ssa_text(result), ty, Some(&text)));
        self.ssa_types.insert(result, ty);
    }

    fn emit_cfg(&mut self, entry: u32, blocks: &[CfgBlock]) -> Result<(), SourceError> {
        let by_id: BTreeMap<u32, &CfgBlock> = blocks.iter().map(|block| (block.id, block)).collect();
        for block in blocks {
            for parameter in &block.parameters {
                let ty = self.scalar_ty(&parameter.ty)?;
                self.ssa_types.insert(parameter.value, ty);
                self.ssa_defs.insert(parameter.value, SsaDef::Param(block.id));
                self.push(&self.decl_line(&self.ssa_text(parameter.value), ty, None));
            }
        }
        for block in blocks {
            match &block.terminator {
                CfgTerminator::Branch { target, args } => self.incoming.entry(*target).or_default().push(args.clone()),
                CfgTerminator::ConditionalBranch { then_block, then_args, else_block, else_args, .. } => {
                    self.incoming.entry(*then_block).or_default().push(then_args.clone());
                    self.incoming.entry(*else_block).or_default().push(else_args.clone());
                }
                CfgTerminator::Return(_) => {}
            }
        }
        let mut emitted = BTreeSet::new();
        let mut current = entry;
        loop {
            let block = *by_id.get(&current).ok_or_else(|| self.invalid(format!("unknown CFG block {current}")))?;
            if !emitted.insert(current) {
                return Err(self.unrecoverable(format!("CFG block {current} is reached more than once")));
            }
            self.cfg_instructions(block)?;
            match &block.terminator {
                CfgTerminator::Return(value) => {
                    self.ssa_type(*value)?;
                    self.push("return;");
                    return Ok(());
                }
                CfgTerminator::Branch { target, args } => {
                    if emitted.contains(target) {
                        return Err(self.unrecoverable(format!("CFG back edge into block {target} is outside the recoverable subset")));
                    }
                    self.bind_parameters(by_id[target], args)?;
                    current = *target;
                }
                CfgTerminator::ConditionalBranch { condition, then_block, then_args, else_block, else_args } => {
                    if self.ssa_type(*condition)? != VType::Bool {
                        return Err(self.abi(format!("CFG branch condition s{condition} is not bool")));
                    }
                    self.block = Some(current);
                    let cond_text = self.ssa_text(*condition);
                    if let Some((finished, merge)) = self.try_cfg_if(&by_id, &emitted, &cond_text, *then_block, then_args, *else_block, else_args)? {
                        if finished {
                            return Ok(());
                        }
                        current = merge;
                        continue;
                    }
                    return Err(self.unrecoverable(format!(
                        "conditional branch at block {current} (targets {then_block}/{else_block}) is outside the recoverable subset"
                    )));
                }
            }
        }
    }

    fn cfg_instructions(&mut self, block: &CfgBlock) -> Result<(), SourceError> {
        self.block = Some(block.id);
        for (index, instruction) in block.instructions.iter().enumerate() {
            self.statement = Some(index as u32);
            self.value = cfg_instruction_value(instruction);
            self.cfg_instruction(instruction)?;
        }
        self.value = None;
        Ok(())
    }

    fn try_cfg_if(
        &mut self,
        by_id: &BTreeMap<u32, &CfgBlock>,
        emitted: &BTreeSet<u32>,
        cond_text: &str,
        then_block: u32,
        then_args: &[u32],
        else_block: u32,
        else_args: &[u32],
    ) -> Result<Option<(bool, u32)>, SourceError> {
        if then_block == else_block || emitted.contains(&then_block) || emitted.contains(&else_block) {
            return Ok(None);
        }
        let then = *by_id.get(&then_block).ok_or_else(|| self.invalid(format!("unknown CFG block {then_block}")))?;
        let else_ = *by_id.get(&else_block).ok_or_else(|| self.invalid(format!("unknown CFG block {else_block}")))?;
        if !then.parameters.is_empty() || !else_.parameters.is_empty() || !then_args.is_empty() || !else_args.is_empty() {
            return Ok(None);
        }
        if self.incoming.get(&then_block).map_or(1, Vec::len) != 1 || self.incoming.get(&else_block).map_or(1, Vec::len) != 1 {
            return Ok(None);
        }
        match (&then.terminator, &else_.terminator) {
            (CfgTerminator::Return(then_value), CfgTerminator::Return(else_value)) => {
                self.push(&format!("if ({cond_text}) {{"));
                self.indent += 1;
                self.cfg_instructions(then)?;
                self.ssa_type(*then_value)?;
                self.push("return;");
                self.indent -= 1;
                self.push("} else {");
                self.indent += 1;
                self.cfg_instructions(else_)?;
                self.ssa_type(*else_value)?;
                self.push("return;");
                self.indent -= 1;
                self.push("}");
                Ok(Some((true, then_block)))
            }
            (
                CfgTerminator::Branch { target: then_target, args: then_edge_args },
                CfgTerminator::Branch { target: else_target, args: else_edge_args },
            ) if then_target == else_target => {
                let merge = *by_id.get(then_target).ok_or_else(|| self.invalid(format!("unknown CFG block {then_target}")))?;
                if emitted.contains(then_target) || then_target == &then_block || then_target == &else_block {
                    return Ok(None);
                }
                if self.incoming.get(then_target).map_or(0, Vec::len) != 2 {
                    return Ok(None);
                }
                self.push(&format!("if ({cond_text}) {{"));
                self.indent += 1;
                self.cfg_instructions(then)?;
                self.bind_parameters(merge, then_edge_args)?;
                self.indent -= 1;
                self.push("} else {");
                self.indent += 1;
                self.cfg_instructions(else_)?;
                self.bind_parameters(merge, else_edge_args)?;
                self.indent -= 1;
                self.push("}");
                Ok(Some((false, *then_target)))
            }
            _ => Ok(None),
        }
    }

    fn bind_parameters(&mut self, target: &CfgBlock, args: &[u32]) -> Result<(), SourceError> {
        if target.parameters.len() != args.len() {
            return Err(self.unrecoverable(format!("CFG edge into block {} carries {} arguments for {} parameters", target.id, args.len(), target.parameters.len())));
        }
        let temps: Vec<(u32, String, VType)> = target
            .parameters
            .iter()
            .zip(args)
            .map(|(parameter, arg)| {
                let ty = self.ssa_type(*arg)?;
                if ty != self.ssa_type(parameter.value)? {
                    return Err(self.abi(format!("CFG parameter s{} type mismatch", parameter.value)));
                }
                Ok((parameter.value, self.ssa_text(*arg), ty))
            })
            .collect::<Result<Vec<_>, SourceError>>()?;
        if temps.iter().all(|(parameter, arg_text, _)| self.ssa_text(*parameter) == *arg_text) {
            return Ok(());
        }
        let mut names = Vec::new();
        for (_, _, ty) in &temps {
            let name = self.next_temp();
            self.push(&self.decl_line(&name, *ty, None));
            names.push(name);
        }
        for (name, (_, arg_text, _)) in names.iter().zip(&temps) {
            self.push(&format!("{name} = {arg_text};"));
        }
        for (parameter, name) in temps.iter().zip(&names) {
            self.push(&format!("{} = {name};", self.ssa_text(parameter.0)));
        }
        Ok(())
    }

    fn cfg_instruction(&mut self, instruction: &CfgInstruction) -> Result<(), SourceError> {
        match instruction {
            CfgInstruction::ConstF32 { result, value } => {
                let text = self.f32_text(*value);
                self.declare_ssa(*result, VType::F32, text);
                Ok(())
            }
            CfgInstruction::ConstF64 { .. } => Err(self.capability("f64 values are outside the text ABI subset".into())),
            CfgInstruction::ConstU32 { result, value } => {
                self.declare_ssa(*result, VType::U32, format!("{value}u"));
                self.ssa_defs.insert(*result, SsaDef::ConstU32(*value));
                Ok(())
            }
            CfgInstruction::LoadUniform { result, slot, ty } => {
                let declared = *self.uniform_slots.get(slot).ok_or_else(|| self.abi(format!("uniform slot {slot} is not declared")))?;
                let want = self.scalar_ty(ty)?;
                let actual = match declared {
                    ParamTy::U32 => VType::U32,
                    ParamTy::F32 => VType::F32,
                };
                if want != actual {
                    return Err(self.abi(format!("uniform slot {slot} is declared {declared:?} but loaded as {ty:?}")));
                }
                let text = self.uniform_text(*slot, declared);
                self.declare_ssa(*result, actual, text);
                Ok(())
            }
            CfgInstruction::LoadBuffer { result, resource, index, ty } => {
                let want = self.scalar_ty(ty)?;
                if want != VType::U32 {
                    return Err(self.abi(format!("resource {resource} elements are u32 but loaded as {ty:?}")));
                }
                let decl = *self.resources.get(resource).ok_or_else(|| self.abi(format!("resource {resource} is not declared")))?;
                if decl.access != ResourceAccess::ReadOnly {
                    return Err(self.abi(format!("resource {resource} is declared write-only but is read")));
                }
                if self.ssa_type(*index)? != VType::U32 {
                    return Err(self.abi("buffer index must be u32".into()));
                }
                let bound = self.ssa_index_bound(*index)?;
                self.record_bound(*resource, bound);
                let text = format!("{}[{}]", decl.name, self.ssa_text(*index));
                self.declare_ssa(*result, VType::U32, text);
                Ok(())
            }
            CfgInstruction::GlobalId { result } => {
                self.declare_ssa(*result, VType::U32, self.id_text().into());
                self.ssa_defs.insert(*result, SsaDef::GlobalId);
                Ok(())
            }
            CfgInstruction::Binary { result, op, lhs, rhs } => {
                let lty = self.ssa_type(*lhs)?;
                let rty = self.ssa_type(*rhs)?;
                let (out_ty, symbol) = self.binop_result(*op, lty, rty)?;
                let text = if matches!(op, BinOp::Min | BinOp::Max | BinOp::Mod | BinOp::Pow | BinOp::Atan2) {
                    self.call_text(symbol, &[self.ssa_text(*lhs), self.ssa_text(*rhs)])
                } else {
                    format!("({} {} {})", self.ssa_text(*lhs), symbol, self.ssa_text(*rhs))
                };
                self.declare_ssa(*result, out_ty, text);
                Ok(())
            }
            CfgInstruction::Unary { result, op, arg } => {
                let ty = self.ssa_type(*arg)?;
                let (out_ty, text) = self.unop_result(*op, ty, &self.ssa_text(*arg))?;
                self.declare_ssa(*result, out_ty, text);
                Ok(())
            }
            CfgInstruction::Vec3Compose { .. } => Err(self.capability("vec3 values are outside the text ABI subset".into())),
            CfgInstruction::Select { result, cond, then_, else_ } => {
                if self.ssa_type(*cond)? != VType::Bool {
                    return Err(self.abi(format!("select condition s{cond} is not bool")));
                }
                let then_ty = self.ssa_type(*then_)?;
                let else_ty = self.ssa_type(*else_)?;
                if then_ty != else_ty {
                    return Err(self.abi(format!("select branches disagree: {then_ty:?} vs {else_ty:?}")));
                }
                let text = match self.target {
                    SourceTarget::Wgsl => format!("select({}, {}, {})", self.ssa_text(*then_), self.ssa_text(*else_), self.ssa_text(*cond)),
                    _ => format!("({} ? {} : {})", self.ssa_text(*cond), self.ssa_text(*then_), self.ssa_text(*else_)),
                };
                if then_ty == VType::Bool {
                    let name = self.next_temp();
                    self.push(&self.decl_line(&name, then_ty, None));
                    self.push(&format!("if ({}) {{", self.ssa_text(*cond)));
                    self.indent += 1;
                    self.push(&format!("{name} = {};", self.ssa_text(*then_)));
                    self.indent -= 1;
                    self.push("} else {");
                    self.indent += 1;
                    self.push(&format!("{name} = {};", self.ssa_text(*else_)));
                    self.indent -= 1;
                    self.push("}");
                    self.declare_ssa(*result, then_ty, name);
                } else {
                    self.declare_ssa(*result, then_ty, text);
                }
                Ok(())
            }
            CfgInstruction::StoreBuffer { resource, index, value } => {
                let decl = *self.resources.get(resource).ok_or_else(|| self.abi(format!("resource {resource} is not declared")))?;
                if decl.access != ResourceAccess::WriteOnly {
                    return Err(self.abi(format!("resource {resource} is declared read-only but is written")));
                }
                if self.ssa_type(*index)? != VType::U32 {
                    return Err(self.abi("buffer index must be u32".into()));
                }
                let vt = self.ssa_type(*value)?;
                if vt != VType::U32 {
                    return Err(self.abi(format!("illegal mixed type: resource {resource} elements are u32 but the stored value is {vt:?}")));
                }
                let bound = self.ssa_index_bound(*index)?;
                self.record_bound(*resource, bound);
                self.push(&format!("{}[{}] = {};", decl.name, self.ssa_text(*index), self.ssa_text(*value)));
                Ok(())
            }
        }
    }
}

fn cfg_instruction_value(instruction: &CfgInstruction) -> Option<u32> {
    match instruction {
        CfgInstruction::ConstF32 { result, .. }
        | CfgInstruction::ConstF64 { result, .. }
        | CfgInstruction::ConstU32 { result, .. }
        | CfgInstruction::LoadUniform { result, .. }
        | CfgInstruction::LoadBuffer { result, .. }
        | CfgInstruction::GlobalId { result }
        | CfgInstruction::Binary { result, .. }
        | CfgInstruction::Unary { result, .. }
        | CfgInstruction::Vec3Compose { result, .. }
        | CfgInstruction::Select { result, .. } => Some(*result),
        CfgInstruction::StoreBuffer { .. } => None,
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn capitalize(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

fn artifact_hash(program: &Program, entry: &str, profile: &str, target: SourceTarget) -> String {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&program_hash(program).to_le_bytes());
    bytes.extend_from_slice(entry.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(profile.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(target.name().as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&ABI_VERSION.to_le_bytes());
    bytes.extend_from_slice(EMITTER_VERSION.as_bytes());
    format!("{:016x}", fnv1a64(&bytes))
}
