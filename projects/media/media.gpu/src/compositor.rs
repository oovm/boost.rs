//! Direct3D 12 compute-shader frame compositor.
//!
//! Every operator mirrors the CPU reference operation by operation:
//!
//! - [`GpuCompositor::resample`] mirrors `media.raster::Paint::resample_from`
//!   (bilinear `Frame::sample`, 1..3-tap box, truncating byte cast, alpha 255).
//! - [`GpuCompositor::vignette`] mirrors `media.post::Vignette::apply`
//!   (`smoothstep(0.25, 0.95, d·√2)` 乘暗, rounding cast, alpha passthrough).
//! - [`GpuCompositor::fill`] / [`GpuCompositor::v_gradient`] /
//!   [`GpuCompositor::radial`] mirror the background painters in
//!   `media.raster::Paint` (`Rgba::lerp` with clamped `t` and rounding cast).
//!
//! Sin/cos, zoom clamping, tap counts and step sizes are computed on the CPU
//! and passed in, so the kernel only runs the per-pixel arithmetic in the same
//! order as the reference loops.
//!
//! The kernel is a UCF [`Program`](hardware_program::Program) emitted to DXBC
//! by `hardware.emitter` and dispatched through `hardware.backend.dx12`; the
//! production path contains no third-party GPU runtime (合同 14 §1.2).

use std::collections::BTreeMap;
use std::fmt;

use hardware_backend_dx12::Dx12Backend;
use hardware_ir::{
    Access, Domain, Graph, Objective, ParamValue, Priority, ResourceGraph, ResourceId, ResourceKind, ResourceNode, ShaderId, TaskGraph, TaskId,
    TaskKind, TaskNode,
};
use hardware_program::{BinOp, FunctionBuilder, Program, ProgramBuilder, Rounding, ScalarTy, Ty, UnOp, ValueId, program_hash};
use hardware_scheduler::Backend as _;
use hardware_types::Dispatch;
use media_image::Frame;

/// Operator selector, matching the shader id used for the kernel.
#[derive(Debug, Clone, Copy)]
enum Op {
    Resample,
    Vignette,
    Fill,
    VGradient,
    Radial,
}

impl Op {
    fn index(self) -> usize {
        match self {
            Self::Resample => 0,
            Self::Vignette => 1,
            Self::Fill => 2,
            Self::VGradient => 3,
            Self::Radial => 4,
        }
    }

    fn shader(self) -> u64 {
        self.index() as u64
    }
}

/// Compute workgroup size in invocations (the emitter maps `GlobalId` to
/// `SV_DispatchThreadID.x`, one pixel per invocation).
const WORKGROUP: u32 = 64;
/// `SV_DispatchThreadID.x` covers 65535 groups of 64 threads; larger frames
/// are walked in tiles with a uniform pixel offset.
const MAX_THREADS: usize = 65_535 * WORKGROUP as usize;

/// Resource ids shared by every kernel.
const SRC: u64 = 0;
const STOPS: u64 = 1;
const DST: u64 = 2;
const SCRATCH: u64 = 3;

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
}

impl GpuCompositor {
    /// Acquire the default D3D12 adapter and emit every operator kernel.
    pub fn new() -> Result<Self, GpuError> {
        Self::acquire()
    }

    /// Kept for hosts that explicitly ask for a software adapter. D3D12 has a
    /// single default device path and WARP is selected by the OS when no
    /// hardware adapter exists, so this matches [`Self::new`].
    pub fn with_fallback_adapter() -> Result<Self, GpuError> {
        Self::acquire()
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

    fn acquire() -> Result<Self, GpuError> {
        if !cfg!(windows) {
            return Err(GpuError::new("DirectX 12 backend requires Windows"));
        }
        let backend = Dx12Backend::new().map_err(|error| GpuError::new(format!("open D3D12 device: {error}")))?;
        let adapter_name = crate::probe::default_adapter_name().unwrap_or_else(|| "D3D12 adapter".to_string());
        let kernels = [Op::Resample, Op::Vignette, Op::Fill, Op::VGradient, Op::Radial]
            .into_iter()
            .map(|op| Kernel::new(&build_program(op)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { backend, kernels, adapter_name })
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
                self.backend = Dx12Backend::new().map_err(|error| GpuError::new(format!("reopen D3D12 device: {error}")))?;
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

fn u32_ty() -> Ty {
    Ty::Scalar(ScalarTy::U32)
}

/// The uniform slots every kernel shares.
const U_WIDTH: u32 = 0;
const U_HEIGHT: u32 = 1;
const U_TILE: u32 = 2;

/// Expression helpers over the UCF program builder.
///
/// Every method performs exactly one builder call; callers sequence operands
/// into locals because the builder takes `&mut self`.
trait Emit {
    fn c_f32(&mut self, value: f32) -> ValueId;
    fn c_u32(&mut self, value: u32) -> ValueId;
    fn uniform_f32(&mut self, slot: u32) -> ValueId;
    fn uniform_u32(&mut self, slot: u32) -> ValueId;
    fn bin(&mut self, op: BinOp, lhs: ValueId, rhs: ValueId) -> ValueId;
    fn un(&mut self, op: UnOp, arg: ValueId, ty: Ty) -> ValueId;
    fn add(&mut self, a: ValueId, b: ValueId) -> ValueId;
    fn sub(&mut self, a: ValueId, b: ValueId) -> ValueId;
    fn mul(&mut self, a: ValueId, b: ValueId) -> ValueId;
    fn div(&mut self, a: ValueId, b: ValueId) -> ValueId;
    fn lt(&mut self, a: ValueId, b: ValueId) -> ValueId;
    fn eq(&mut self, a: ValueId, b: ValueId) -> ValueId;
    fn select(&mut self, cond: ValueId, then_: ValueId, else_: ValueId) -> ValueId;
    fn min_u32(&mut self, a: ValueId, b: ValueId) -> ValueId;
    fn max_f32(&mut self, a: ValueId, b: ValueId) -> ValueId;
    fn min_f32(&mut self, a: ValueId, b: ValueId) -> ValueId;
    fn to_f32(&mut self, value: ValueId) -> ValueId;
    fn to_u32_trunc(&mut self, value: ValueId) -> ValueId;
    fn to_u32_round(&mut self, value: ValueId) -> ValueId;
    fn channel_u32(&mut self, word: ValueId, index: u32) -> ValueId;
    fn channel_f32(&mut self, word: ValueId, index: u32) -> ValueId;
    fn pack(&mut self, r: ValueId, g: ValueId, b: ValueId, a: ValueId) -> ValueId;
    fn to8_trunc(&mut self, value: ValueId) -> ValueId;
    fn to8_round(&mut self, value: ValueId) -> ValueId;
    fn lerp_word(&mut self, a: ValueId, b: ValueId, t: ValueId) -> ValueId;
}

impl Emit for FunctionBuilder {
    fn c_f32(&mut self, value: f32) -> ValueId {
        self.emit_const_f32(value).expect("open block")
    }
    fn c_u32(&mut self, value: u32) -> ValueId {
        self.emit_const_u32(value).expect("open block")
    }
    fn uniform_f32(&mut self, slot: u32) -> ValueId {
        self.emit_load_uniform(slot, Ty::f32()).expect("open block")
    }
    fn uniform_u32(&mut self, slot: u32) -> ValueId {
        let value = self.uniform_f32(slot);
        self.to_u32_trunc(value)
    }
    fn bin(&mut self, op: BinOp, lhs: ValueId, rhs: ValueId) -> ValueId {
        self.emit_binary(op, lhs, rhs).expect("matching types")
    }
    fn un(&mut self, op: UnOp, arg: ValueId, ty: Ty) -> ValueId {
        self.emit_unary(op, arg, ty).expect("matching types")
    }
    fn add(&mut self, a: ValueId, b: ValueId) -> ValueId {
        self.bin(BinOp::Add, a, b)
    }
    fn sub(&mut self, a: ValueId, b: ValueId) -> ValueId {
        self.bin(BinOp::Sub, a, b)
    }
    fn mul(&mut self, a: ValueId, b: ValueId) -> ValueId {
        self.bin(BinOp::Mul, a, b)
    }
    fn div(&mut self, a: ValueId, b: ValueId) -> ValueId {
        self.bin(BinOp::Div, a, b)
    }
    fn lt(&mut self, a: ValueId, b: ValueId) -> ValueId {
        self.bin(BinOp::Lt, a, b)
    }
    fn eq(&mut self, a: ValueId, b: ValueId) -> ValueId {
        self.bin(BinOp::Eq, a, b)
    }
    fn select(&mut self, cond: ValueId, then_: ValueId, else_: ValueId) -> ValueId {
        self.emit_select(cond, then_, else_).expect("matching types")
    }
    fn min_u32(&mut self, a: ValueId, b: ValueId) -> ValueId {
        let cond = self.lt(a, b);
        self.select(cond, a, b)
    }
    fn max_f32(&mut self, a: ValueId, b: ValueId) -> ValueId {
        self.bin(BinOp::Max, a, b)
    }
    fn min_f32(&mut self, a: ValueId, b: ValueId) -> ValueId {
        self.bin(BinOp::Min, a, b)
    }
    fn to_f32(&mut self, value: ValueId) -> ValueId {
        self.un(UnOp::U32ToF32, value, Ty::f32())
    }
    fn to_u32_trunc(&mut self, value: ValueId) -> ValueId {
        self.un(UnOp::F32ToU32(Rounding::Trunc), value, u32_ty())
    }
    fn to_u32_round(&mut self, value: ValueId) -> ValueId {
        self.un(UnOp::F32ToU32(Rounding::Round), value, u32_ty())
    }
    fn channel_u32(&mut self, word: ValueId, index: u32) -> ValueId {
        let shifted = if index == 0 {
            word
        }
        else {
            let shift = self.c_u32(index * 8);
            self.bin(BinOp::Shr, word, shift)
        };
        let mask = self.c_u32(0xFF);
        self.bin(BinOp::And, shifted, mask)
    }
    fn channel_f32(&mut self, word: ValueId, index: u32) -> ValueId {
        let value = self.channel_u32(word, index);
        self.to_f32(value)
    }
    fn pack(&mut self, r: ValueId, g: ValueId, b: ValueId, a: ValueId) -> ValueId {
        let shift = self.c_u32(8);
        let g = self.bin(BinOp::Shl, g, shift);
        let shift = self.c_u32(16);
        let b = self.bin(BinOp::Shl, b, shift);
        let shift = self.c_u32(24);
        let a = self.bin(BinOp::Shl, a, shift);
        let rg = self.bin(BinOp::Or, r, g);
        let ba = self.bin(BinOp::Or, b, a);
        self.bin(BinOp::Or, rg, ba)
    }
    fn to8_trunc(&mut self, value: ValueId) -> ValueId {
        let lo = self.c_f32(0.0);
        let hi = self.c_f32(255.0);
        let above = self.max_f32(value, lo);
        let clamped = self.min_f32(above, hi);
        self.to_u32_trunc(clamped)
    }
    fn to8_round(&mut self, value: ValueId) -> ValueId {
        let lo = self.c_f32(0.0);
        let hi = self.c_f32(255.0);
        let above = self.max_f32(value, lo);
        let clamped = self.min_f32(above, hi);
        self.to_u32_round(clamped)
    }
    fn lerp_word(&mut self, a: ValueId, b: ValueId, t: ValueId) -> ValueId {
        let lo = self.c_f32(0.0);
        let hi = self.c_f32(1.0);
        let below = self.max_f32(t, lo);
        let t = self.min_f32(below, hi);
        let mut channels: [ValueId; 4] = [a; 4];
        for (index, channel) in channels.iter_mut().enumerate() {
            let ca = self.channel_f32(a, index as u32);
            let cb = self.channel_f32(b, index as u32);
            let delta = self.sub(cb, ca);
            let offset = self.mul(delta, t);
            let mixed = self.add(ca, offset);
            let byte = self.to8_round(mixed);
            *channel = byte;
        }
        self.pack(channels[0], channels[1], channels[2], channels[3])
    }
}

/// Bilinear sample of the source frame in fractional pixel coordinates,
/// channels as display floats 0..255, same order as `media_image::Frame::sample`.
fn sample(f: &mut FunctionBuilder, sx: ValueId, sy: ValueId, sw: ValueId, sh: ValueId) -> [ValueId; 3] {
    let zero = f.c_f32(0.0);
    let one = f.c_u32(1);
    let wm = f.sub(sw, one);
    let hm = f.sub(sh, one);
    let wm_f = f.to_f32(wm);
    let hm_f = f.to_f32(hm);
    let above = f.max_f32(sx, zero);
    let x = f.min_f32(above, wm_f);
    let above = f.max_f32(sy, zero);
    let y = f.min_f32(above, hm_f);
    let x0 = f.to_u32_trunc(x);
    let y0 = f.to_u32_trunc(y);
    let x0_f = f.to_f32(x0);
    let y0_f = f.to_f32(y0);
    let fx = f.sub(x, x0_f);
    let fy = f.sub(y, y0_f);
    let x0p1 = f.add(x0, one);
    let y0p1 = f.add(y0, one);
    let x1 = f.min_u32(x0p1, wm);
    let y1 = f.min_u32(y0p1, hm);
    let row0 = f.mul(y0, sw);
    let row1 = f.mul(y1, sw);
    let i00 = f.add(row0, x0);
    let i10 = f.add(row0, x1);
    let i01 = f.add(row1, x0);
    let i11 = f.add(row1, x1);
    let c00 = f.emit_load_buffer(SRC as u32, i00, u32_ty()).expect("load");
    let c10 = f.emit_load_buffer(SRC as u32, i10, u32_ty()).expect("load");
    let c01 = f.emit_load_buffer(SRC as u32, i01, u32_ty()).expect("load");
    let c11 = f.emit_load_buffer(SRC as u32, i11, u32_ty()).expect("load");
    let mut out: [ValueId; 3] = [c00; 3];
    for (index, channel) in out.iter_mut().enumerate() {
        let p00 = f.channel_f32(c00, index as u32);
        let p10 = f.channel_f32(c10, index as u32);
        let p01 = f.channel_f32(c01, index as u32);
        let p11 = f.channel_f32(c11, index as u32);
        let d10 = f.sub(p10, p00);
        let w10 = f.mul(d10, fx);
        let top = f.add(p00, w10);
        let d11 = f.sub(p11, p01);
        let w11 = f.mul(d11, fx);
        let bottom = f.add(p01, w11);
        let dv = f.sub(bottom, top);
        let wv = f.mul(dv, fy);
        *channel = f.add(top, wv);
    }
    out
}

/// Builds one operator kernel.
fn build_program(op: Op) -> Program {
    let mut f = FunctionBuilder::new("composite");
    let gid = f.emit_global_id().expect("global id");
    let tile = f.uniform_u32(U_TILE);
    let index = f.add(gid, tile);
    let width_u = f.uniform_u32(U_WIDTH);
    let height_u = f.uniform_u32(U_HEIGHT);
    let total = f.mul(width_u, height_u);
    let inside = f.lt(index, total);
    let body = f.create_block();
    let exit = f.create_block();
    f.conditional_branch(inside, body, exit).expect("branch");

    f.switch_to(body).expect("body");
    let x = f.bin(BinOp::Mod, index, width_u);
    let y = f.bin(BinOp::Div, index, width_u);
    let value = emit_operator(&mut f, op, index, x, y, width_u, height_u);
    f.emit_store_buffer(DST as u32, index, value).expect("store");
    // The DXBC emitter only supports scalar `f32` returns; the return lane is
    // the dispatch's scratch UAV and is not read back.
    let zero = f.c_f32(0.0);
    f.return_value(zero).expect("return");

    f.switch_to(exit).expect("exit");
    let zero = f.c_f32(0.0);
    f.return_value(zero).expect("return");

    ProgramBuilder::new().entry(f).expect("entry").finish().expect("valid kernel")
}

fn emit_operator(f: &mut FunctionBuilder, op: Op, index: ValueId, x: ValueId, y: ValueId, width_u: ValueId, height_u: ValueId) -> ValueId {
    match op {
        Op::Fill => {
            let r = f.uniform_u32(3);
            let g = f.uniform_u32(4);
            let b = f.uniform_u32(5);
            let a = f.uniform_u32(6);
            f.pack(r, g, b, a)
        }
        Op::VGradient => {
            let y_f = f.to_f32(y);
            let stops = f.uniform_u32(3);
            let segments = f.uniform_f32(4);
            let step = f.uniform_f32(5);
            let scaled = f.div(y_f, step);
            let t = f.mul(scaled, segments);
            let raw = f.to_u32_trunc(t);
            let two = f.c_u32(2);
            let max_seg = f.sub(stops, two);
            let seg = f.min_u32(raw, max_seg);
            let seg_f = f.to_f32(seg);
            let local = f.sub(t, seg_f);
            let one = f.c_u32(1);
            let next = f.add(seg, one);
            let a = f.emit_load_buffer(STOPS as u32, seg, u32_ty()).expect("stop");
            let b = f.emit_load_buffer(STOPS as u32, next, u32_ty()).expect("stop");
            f.lerp_word(a, b, local)
        }
        Op::Radial => {
            let x_f = f.to_f32(x);
            let y_f = f.to_f32(y);
            let width_f = f.to_f32(width_u);
            let height_f = f.to_f32(height_u);
            let two = f.c_f32(2.0);
            let cx = f.div(width_f, two);
            let cy = f.div(height_f, two);
            let cx2 = f.mul(cx, cx);
            let cy2 = f.mul(cy, cy);
            let sum = f.add(cx2, cy2);
            let max_r = f.un(UnOp::Sqrt, sum, Ty::f32());
            let dx = f.sub(x_f, cx);
            let dy = f.sub(y_f, cy);
            let dx2 = f.mul(dx, dx);
            let dy2 = f.mul(dy, dy);
            let sum = f.add(dx2, dy2);
            let dist = f.un(UnOp::Sqrt, sum, Ty::f32());
            let t = f.div(dist, max_r);
            let mut center: [ValueId; 4] = [width_u; 4];
            let mut edge: [ValueId; 4] = [height_u; 4];
            for (slot, channel) in center.iter_mut().enumerate() {
                let value = f.uniform_u32(3 + slot as u32);
                *channel = value;
            }
            for (slot, channel) in edge.iter_mut().enumerate() {
                let value = f.uniform_u32(7 + slot as u32);
                *channel = value;
            }
            let center = f.pack(center[0], center[1], center[2], center[3]);
            let edge = f.pack(edge[0], edge[1], edge[2], edge[3]);
            f.lerp_word(center, edge, t)
        }
        Op::Vignette => {
            let word = f.emit_load_buffer(SRC as u32, index, u32_ty()).expect("load");
            let x_f = f.to_f32(x);
            let y_f = f.to_f32(y);
            let half = f.c_f32(0.5);
            let width_f = f.to_f32(width_u);
            let height_f = f.to_f32(height_u);
            let shifted = f.add(x_f, half);
            let u = f.div(shifted, width_f);
            let shifted = f.add(y_f, half);
            let v = f.div(shifted, height_f);
            let du = f.sub(u, half);
            let dv = f.sub(v, half);
            let du2 = f.mul(du, du);
            let dv2 = f.mul(dv, dv);
            let sum = f.add(du2, dv2);
            let root = f.un(UnOp::Sqrt, sum, Ty::f32());
            let root2 = f.c_f32(std::f32::consts::SQRT_2);
            let dist = f.mul(root, root2);
            let lo = f.c_f32(0.25);
            let span = f.c_f32(0.7);
            let over = f.sub(dist, lo);
            let raw = f.div(over, span);
            let zero = f.c_f32(0.0);
            let one = f.c_f32(1.0);
            let above = f.max_f32(raw, zero);
            let t = f.min_f32(above, one);
            let three = f.c_f32(3.0);
            let two = f.c_f32(2.0);
            let t2 = f.mul(t, t);
            let tt = f.mul(two, t);
            let inner = f.sub(three, tt);
            let edge = f.mul(t2, inner);
            let strength = f.uniform_f32(3);
            let dark = f.mul(strength, edge);
            let factor = f.sub(one, dark);
            let r0 = f.channel_f32(word, 0);
            let r1 = f.mul(r0, factor);
            let r = f.to8_round(r1);
            let g0 = f.channel_f32(word, 1);
            let g1 = f.mul(g0, factor);
            let g = f.to8_round(g1);
            let b0 = f.channel_f32(word, 2);
            let b1 = f.mul(b0, factor);
            let b = f.to8_round(b1);
            let a = f.channel_u32(word, 3);
            f.pack(r, g, b, a)
        }
        Op::Resample => {
            let src_w = f.uniform_u32(3);
            let src_h = f.uniform_u32(4);
            let zoom = f.uniform_f32(5);
            let focus_x = f.uniform_f32(6);
            let focus_y = f.uniform_f32(7);
            let taps = f.uniform_u32(8);
            let step = f.uniform_f32(9);
            let sin = f.uniform_f32(10);
            let cos = f.uniform_f32(11);
            let x_f = f.to_f32(x);
            let y_f = f.to_f32(y);
            let half = f.c_f32(0.5);
            let width_f = f.to_f32(width_u);
            let height_f = f.to_f32(height_u);
            let shifted = f.add(x_f, half);
            let u0 = f.div(shifted, width_f);
            let u = f.sub(u0, half);
            let shifted = f.add(y_f, half);
            let v0 = f.div(shifted, height_f);
            let v = f.sub(v0, half);
            let zu = f.div(u, zoom);
            let zv = f.div(v, zoom);
            let zu_cos = f.mul(zu, cos);
            let zv_sin = f.mul(zv, sin);
            let ru = f.add(zu_cos, zv_sin);
            let neg_sin = f.un(UnOp::Neg, sin, Ty::f32());
            let zu_neg = f.mul(zu, neg_sin);
            let zv_cos = f.mul(zv, cos);
            let rv = f.add(zu_neg, zv_cos);
            let src_w_f = f.to_f32(src_w);
            let src_h_f = f.to_f32(src_h);
            let fx = f.add(focus_x, ru);
            let fx = f.mul(fx, src_w_f);
            let sx = f.sub(fx, half);
            let fx = f.add(focus_y, rv);
            let fx = f.mul(fx, src_h_f);
            let sy = f.sub(fx, half);

            let single = sample(f, sx, sy, src_w, src_h);
            let zero_acc = f.c_f32(0.0);

            let half_step = f.mul(step, half);
            let neg_half = f.un(UnOp::Neg, half_step, Ty::f32());
            let offsets2 = [
                (neg_half, neg_half),
                (half_step, neg_half),
                (neg_half, half_step),
                (half_step, half_step),
            ];
            let mut acc2: [ValueId; 3] = [zero_acc; 3];
            for (dx, dy) in offsets2 {
                let tx = f.add(sx, dx);
                let ty = f.add(sy, dy);
                let tap = sample(f, tx, ty, src_w, src_h);
                for (channel, value) in acc2.iter_mut().enumerate() {
                    let sum = f.add(*value, tap[channel]);
                    *value = sum;
                }
            }
            let four = f.c_f32(4.0);
            let mut pair: [ValueId; 3] = [zero_acc; 3];
            for (channel, value) in pair.iter_mut().enumerate() {
                let avg = f.div(acc2[channel], four);
                *value = avg;
            }

            let neg_step = f.un(UnOp::Neg, step, Ty::f32());
            let zero = f.c_f32(0.0);
            let offsets3 = [
                (neg_step, neg_step),
                (zero, neg_step),
                (step, neg_step),
                (neg_step, zero),
                (zero, zero),
                (step, zero),
                (neg_step, step),
                (zero, step),
                (step, step),
            ];
            let mut acc3: [ValueId; 3] = [zero_acc; 3];
            for (dx, dy) in offsets3 {
                let tx = f.add(sx, dx);
                let ty = f.add(sy, dy);
                let tap = sample(f, tx, ty, src_w, src_h);
                for (channel, value) in acc3.iter_mut().enumerate() {
                    let sum = f.add(*value, tap[channel]);
                    *value = sum;
                }
            }
            let nine = f.c_f32(9.0);
            let mut triple: [ValueId; 3] = [zero_acc; 3];
            for (channel, value) in triple.iter_mut().enumerate() {
                let avg = f.div(acc3[channel], nine);
                *value = avg;
            }

            let one_u = f.c_u32(1);
            let two_u = f.c_u32(2);
            let is_one = f.eq(taps, one_u);
            let is_two = f.eq(taps, two_u);
            let mut channels: [ValueId; 3] = [zero_acc; 3];
            for (channel, value) in channels.iter_mut().enumerate() {
                let two_or_three = f.select(is_two, pair[channel], triple[channel]);
                let chosen = f.select(is_one, single[channel], two_or_three);
                *value = chosen;
            }
            let r = f.to8_trunc(channels[0]);
            let g = f.to8_trunc(channels[1]);
            let b = f.to8_trunc(channels[2]);
            let a = f.c_u32(255);
            f.pack(r, g, b, a)
        }
    }
}
