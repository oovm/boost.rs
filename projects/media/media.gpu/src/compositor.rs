//! Direct3D 12 compute-shader frame compositor.
//!
//! The kernels themselves live in [`crate::kernels`], so the same UCF programs
//! feed the D3D12 path here and the build-time WGSL bundles. Every operator
//! mirrors the CPU reference operation by operation; see that module for the
//! per-operator notes.
//!
//! The kernel is emitted to DXBC by `hardware.emitter` and dispatched through
//! `hardware.backend.dx12`; the production path contains no third-party GPU
//! runtime (合同 14 §1.2).

use std::collections::BTreeMap;
use std::fmt;

use hardware_backend_dx12::Dx12Backend;
use hardware_ir::{
    Access, Domain, Graph, Objective, ParamValue, Priority, ResourceGraph, ResourceId, ResourceKind, ResourceNode, ShaderId, TaskGraph, TaskId,
    TaskKind, TaskNode,
};
use hardware_program::{Program, program_hash};
use hardware_scheduler::Backend as _;
use hardware_types::Dispatch;
use media_image::Frame;

use crate::kernels::{DST, Op, SCRATCH, SRC, STOPS, WORKGROUP, build_program};

/// `SV_DispatchThreadID.x` covers 65535 groups of 64 threads; larger frames
/// are walked in tiles with a uniform pixel offset.
const MAX_THREADS: usize = 65_535 * WORKGROUP as usize;

/// A GPU compositor failure: no adapter, device loss or a failed readback.
#[derive(Debug, Clone)]
pub struct GpuError {
    /// What failed, in one sentence.
    pub message: String,
}

impl GpuError {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "gpu compositor: {}", self.message)
    }
}

impl std::error::Error for GpuError {}

/// One emitted kernel plus the serialized body the backend expects.
struct Kernel {
    hash: u64,
    body: String,
}

impl Kernel {
    fn new(program: &Program) -> Result<Self, GpuError> {
        let body = serde_json::to_string(program).map_err(|error| GpuError::new(format!("serialize kernel: {error}")))?;
        Ok(Self { hash: program_hash(program), body })
    }
}

/// The Direct3D 12 compositor: one device, one emitted kernel per operator.
///
/// Holds no frame state — frames enter and leave as [`media_image::Frame`]
/// values, so the CPU path and the GPU path share the same storage type.
pub struct GpuCompositor {
    backend: Dx12Backend,
    kernels: Vec<Kernel>,
    adapter_name: String,
    /// Whether a software adapter is acceptable; replayed when the device is
    /// reopened for a new frame shape.
    allow_software: bool,
}

impl GpuCompositor {
    /// Acquire a hardware D3D12 adapter and emit every operator kernel.
    ///
    /// Software adapters (`WARP` / Microsoft Basic Render Driver) are refused:
    /// dispatching on them can crash inside the driver. On a host that only
    /// exposes a software adapter this returns `Err`, and callers should stay
    /// on the CPU path. Use [`Self::with_fallback_adapter`] to opt in instead.
    pub fn new() -> Result<Self, GpuError> {
        Self::acquire(false)
    }

    /// Acquire a D3D12 adapter, explicitly allowing the software fallback
    /// (`WARP` / Microsoft Basic Render Driver) when no hardware adapter exists.
    ///
    /// The software path is best effort: it is slow and, on some hosts, cannot
    /// dispatch at all. Prefer [`Self::new`] and only fall back when the CPU
    /// path is unusable.
    pub fn with_fallback_adapter() -> Result<Self, GpuError> {
        Self::acquire(true)
    }

    /// The adapter name, e.g. for `RenderStats` provenance.
    #[must_use]
    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    /// The backend label (`D3D12`), e.g. for provenance.
    #[must_use]
    pub fn backend(&self) -> &str {
        "D3D12"
    }

    fn acquire(allow_software: bool) -> Result<Self, GpuError> {
        if !cfg!(windows) {
            return Err(GpuError::new("DirectX 12 backend requires Windows"));
        }
        let backend = Self::open(allow_software)?;
        let adapter_name = crate::probe::default_adapter_name().unwrap_or_else(|| "D3D12 adapter".to_string());
        let kernels = Op::ALL
            .into_iter()
            .map(|op| Kernel::new(&build_program(op)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { backend, kernels, adapter_name, allow_software })
    }

    fn open(allow_software: bool) -> Result<Dx12Backend, GpuError> {
        let opened = if allow_software { Dx12Backend::new_allowing_software() } else { Dx12Backend::new() };
        opened.map_err(|error| GpuError::new(format!("open D3D12 device: {error}")))
    }

    /// Paints the whole frame with one solid color (background contract).
    pub fn fill(&mut self, frame: &mut Frame, color: [u8; 4]) -> Result<(), GpuError> {
        let mut params = vec![0.0; 4];
        for (slot, channel) in color.iter().enumerate() {
            params[slot] = f32::from(*channel);
        }
        let pixels = self.run(Op::Fill, frame.width, frame.height, None, &[], &params)?;
        frame.pixels = pixels;
        Ok(())
    }

    /// Vertical gradient through evenly spaced stops (background contract).
    ///
    /// The stop list mirrors `Paint::v_gradient`: empty is a no-op, a single
    /// stop is a fill, otherwise per-row `Rgba::lerp` with rounding cast.
    pub fn v_gradient(&mut self, frame: &mut Frame, stops: &[[u8; 4]]) -> Result<(), GpuError> {
        if stops.is_empty() {
            return Ok(());
        }
        if stops.len() == 1 {
            return self.fill(frame, stops[0]);
        }
        let params = vec![stops.len() as f32, (stops.len() - 1) as f32, frame.height.saturating_sub(1).max(1) as f32];
        let pixels = self.run(Op::VGradient, frame.width, frame.height, None, stops, &params)?;
        frame.pixels = pixels;
        Ok(())
    }

    /// Radial gradient center→edge (background contract).
    pub fn radial(&mut self, frame: &mut Frame, center: [u8; 4], edge: [u8; 4]) -> Result<(), GpuError> {
        let mut params = vec![0.0; 8];
        for (slot, channel) in center.iter().enumerate() {
            params[slot] = f32::from(*channel);
        }
        for (slot, channel) in edge.iter().enumerate() {
            params[4 + slot] = f32::from(*channel);
        }
        let pixels = self.run(Op::Radial, frame.width, frame.height, None, &[], &params)?;
        frame.pixels = pixels;
        Ok(())
    }

    /// Radial vignette: the one authored darkening formula
    /// (`media.post::Vignette`), alpha passthrough.
    pub fn vignette(&mut self, frame: &mut Frame, strength: f32) -> Result<(), GpuError> {
        let strength = strength.clamp(0.0, 1.0);
        if strength <= 0.0 {
            return Ok(());
        }
        let pixels = self.run(Op::Vignette, frame.width, frame.height, Some(frame), &[], &[strength])?;
        frame.pixels = pixels;
        Ok(())
    }

    /// Camera resample: samples `src` around `focus` at `zoom` and optional
    /// rotation into `dst`, reproducing `Paint::resample_from` — including
    /// the truncating byte cast and the constant opaque alpha.
    pub fn resample(&mut self, dst: &mut Frame, src: &Frame, zoom: f32, focus: [f32; 2], rotate_deg: f32) -> Result<(), GpuError> {
        let zoom = zoom.max(0.05);
        let angle = rotate_deg.to_radians();
        let (sin, cos) = angle.sin_cos();
        let ratio = src.width as f32 / (dst.width as f32 * zoom);
        let taps = (ratio.ceil() as u32).clamp(1, 3);
        let step = ratio / taps as f32;
        let params = vec![
            src.width as f32,
            src.height as f32,
            zoom,
            focus[0],
            focus[1],
            taps as f32,
            step,
            sin,
            cos,
        ];
        let pixels = self.run(Op::Resample, dst.width, dst.height, Some(src), &[], &params)?;
        dst.pixels = pixels;
        Ok(())
    }

    /// Runs one operator over the whole output frame and returns its pixels.
    fn run(&mut self, op: Op, width: u32, height: u32, src: Option<&Frame>, stops: &[[u8; 4]], params: &[f32]) -> Result<Vec<u8>, GpuError> {
        let total = (width as usize) * (height as usize);
        if total == 0 {
            return Ok(Vec::new());
        }
        let dst_bytes = total * 4;
        let scratch_elems = total.min(MAX_THREADS);
        let source_bytes = src.map(|frame| frame.pixels.len()).unwrap_or(0);
        let stop_bytes = stops.len() * 4;

        let mut nodes = Vec::new();
        if src.is_some() {
            nodes.push(resource(SRC, source_bytes));
        }
        if !stops.is_empty() {
            nodes.push(resource(STOPS, stop_bytes));
        }
        nodes.push(resource(DST, dst_bytes));
        nodes.push(resource(SCRATCH, scratch_elems * 4));

        let base = Graph {
            resources: ResourceGraph { nodes },
            tasks: TaskGraph { nodes: Vec::new(), edges: Vec::new() },
        };
        self.prepare(&base)?;

        if let Some(frame) = src {
            self.backend
                .write_f32(ResourceId(SRC), &words(&frame.pixels))
                .map_err(|error| GpuError::new(format!("upload source frame: {error}")))?;
        }
        if !stops.is_empty() {
            let packed: Vec<u8> = stops.iter().flat_map(|stop| *stop).collect();
            self.backend
                .write_f32(ResourceId(STOPS), &words(&packed))
                .map_err(|error| GpuError::new(format!("upload gradient stops: {error}")))?;
        }

        let kernel = &self.kernels[op.index()];
        let mut offset = 0usize;
        while offset < total {
            let chunk = (total - offset).min(MAX_THREADS);
            let mut values = Vec::with_capacity(3 + params.len());
            values.push(width as f32);
            values.push(height as f32);
            values.push(offset as f32);
            values.extend_from_slice(params);

            let mut task_params = BTreeMap::new();
            task_params.insert(
                "program".to_string(),
                ParamValue::Program { version: 1, hash: kernel.hash, body: kernel.body.clone() },
            );
            task_params.insert("output".to_string(), ParamValue::I64(SCRATCH as i64));
            if src.is_some() {
                task_params.insert(format!("buffer_{SRC}"), ParamValue::I64(SRC as i64));
            }
            if !stops.is_empty() {
                task_params.insert(format!("buffer_{STOPS}"), ParamValue::I64(STOPS as i64));
            }
            task_params.insert(format!("buffer_{DST}"), ParamValue::I64(DST as i64));
            for (slot, value) in values.iter().enumerate() {
                task_params.insert(format!("uniform_{slot}"), ParamValue::F64(f64::from(*value)));
            }

            let graph = Graph {
                resources: ResourceGraph { nodes: base.resources.nodes.clone() },
                tasks: TaskGraph {
                    nodes: vec![TaskNode {
                        id: TaskId(offset as u64),
                        kind: TaskKind::Dispatch,
                        shader: ShaderId(op.shader()),
                        params: task_params,
                        // `Explicit` counts threads; the backend groups them in 64s.
                        dispatch: Dispatch::Explicit { x: chunk as u32, y: 1, z: 1 },
                        objective: Objective::MaxThroughput,
                        priority: Priority::Batch,
                    }],
                    edges: Vec::new(),
                },
            };
            self.backend
                .run_prepared(&graph)
                .map_err(|error| GpuError::new(format!("dispatch {}: {error}", op.index())))?;
            offset += chunk;
        }

        let mut out = self
            .backend
            .read_u8(ResourceId(DST))
            .map_err(|error| GpuError::new(format!("read back frame: {error}")))?;
        out.truncate(dst_bytes);
        Ok(out)
    }

    /// Allocates the frame buffers, recreating the device when the frame shape
    /// changes (the D3D12 backend pins buffer sizes per resource id).
    fn prepare(&mut self, graph: &Graph) -> Result<(), GpuError> {
        match self.backend.prepare(graph) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.backend = Self::open(self.allow_software)?;
                self.backend
                    .prepare(graph)
                    .map_err(|error| GpuError::new(format!("allocate frame buffers: {error}")))
            }
        }
    }
}

/// A device buffer for one resource id.
fn resource(id: u64, bytes: usize) -> ResourceNode {
    ResourceNode {
        id: ResourceId(id),
        kind: ResourceKind::Buffer,
        domain: Domain::Vram,
        access: Access::ReadWrite,
        byte_size: Some(bytes as u64),
    }
}

/// Reinterprets packed little-endian bytes as the `f32` lanes `write_f32`
/// uploads. The kernel reads them back as `u32` words, so the bit pattern is
/// preserved exactly.
fn words(bytes: &[u8]) -> Vec<f32> {
    bytes.chunks_exact(4).map(|chunk| f32::from_bits(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))).collect()
}
