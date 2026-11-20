//! Compute-shader frame compositor.
//!
//! Every operator mirrors the CPU reference operation by operation:
//!
//! - [`GpuCompositor::resample`] mirrors `media.raster::Paint::resample_from`
//!   (bilinear `Frame::sample`, 1..3-tap box, truncating byte cast, alpha 255).
//! - [`GpuCompositor::vignette`] mirrors `media.post::Vignette::apply`
//!   (`smoothstep(0.25, 0.95, d·√2)`乘暗, rounding cast, alpha passthrough).
//! - [`GpuCompositor::fill`] / [`GpuCompositor::v_gradient`] /
//!   [`GpuCompositor::radial`] mirror the background painters in
//!   `media.raster::Paint` (`Rgba::lerp` with clamped `t` and rounding cast).
//!
//! Sin/cos, zoom clamping, tap counts and step sizes are computed on the CPU
//! and passed in, so the shader only runs the per-pixel arithmetic in the
//! same order as the reference loops.

use media_image::Frame;
use std::fmt;
#[cfg(feature = "gpu")]
use wgpu::util::DeviceExt as _;

/// Operator selector compiled into the shader.
const OP_RESAMPLE: u32 = 0;
const OP_VIGNETTE: u32 = 1;
const OP_FILL: u32 = 2;
const OP_V_GRADIENT: u32 = 3;
const OP_RADIAL: u32 = 4;

/// Compute workgroup edge in pixels (8×8 invocations).
const WORKGROUP: u32 = 8;

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

/// The uniform block shared with WGSL. Field order is the shader contract;
/// every member is a 4-byte scalar so the layout has no padding holes.
#[derive(Clone, Copy)]
struct Params {
    width: u32,
    height: u32,
    src_width: u32,
    src_height: u32,
    op: u32,
    taps: u32,
    stop_count: u32,
    zoom: f32,
    focus_x: f32,
    focus_y: f32,
    /// Resample: tap step in source pixels. Gradient: `(h-1).max(1)` row
    /// denominator. Both are per-op scalars reused through one slot.
    step: f32,
    sin: f32,
    cos: f32,
    strength: f32,
    segments: f32,
    color: u32,
    center_color: u32,
    edge_color: u32,
    /// Pixel offset of the current dispatch tile (grid-stride over the
    /// adapter's `max_compute_workgroups_per_dimension`).
    tile_x: u32,
    tile_y: u32,
}

impl Params {
    fn op(op: u32, width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            src_width: width,
            src_height: height,
            op,
            taps: 1,
            stop_count: 0,
            zoom: 1.0,
            focus_x: 0.5,
            focus_y: 0.5,
            step: 1.0,
            sin: 0.0,
            cos: 1.0,
            strength: 0.0,
            segments: 0.0,
            color: 0,
            center_color: 0,
            edge_color: 0,
            tile_x: 0,
            tile_y: 0,
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(20 * 4);
        for word in [
            self.width.to_le_bytes(),
            self.height.to_le_bytes(),
            self.src_width.to_le_bytes(),
            self.src_height.to_le_bytes(),
            self.op.to_le_bytes(),
            self.taps.to_le_bytes(),
            self.stop_count.to_le_bytes(),
            self.zoom.to_le_bytes(),
            self.focus_x.to_le_bytes(),
            self.focus_y.to_le_bytes(),
            self.step.to_le_bytes(),
            self.sin.to_le_bytes(),
            self.cos.to_le_bytes(),
            self.strength.to_le_bytes(),
            self.segments.to_le_bytes(),
            self.color.to_le_bytes(),
            self.center_color.to_le_bytes(),
            self.edge_color.to_le_bytes(),
            self.tile_x.to_le_bytes(),
            self.tile_y.to_le_bytes(),
        ] {
            out.extend_from_slice(&word);
        }
        out
    }
}

/// Packs display bytes into the shader's `u32` lane order (little-endian).
fn pack_rgba(c: [u8; 4]) -> u32 {
    u32::from(c[0]) | (u32::from(c[1]) << 8) | (u32::from(c[2]) << 16) | (u32::from(c[3]) << 24)
}

/// The wgpu compositor: one device, one compute pipeline, per-call buffers.
///
/// Holds no frame state — frames enter and leave as [`media_image::Frame`]
/// values, so the CPU path and the GPU path share the same storage type.
#[cfg(feature = "gpu")]
pub struct GpuCompositor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    bind_layout: wgpu::BindGroupLayout,
    adapter_name: String,
    backend: String,
}

#[cfg(feature = "gpu")]
impl GpuCompositor {
    /// Acquire a high-performance adapter and build the pipeline.
    pub fn new() -> Result<Self, GpuError> {
        Self::acquire(false)
    }

    /// Acquire the fallback adapter (software rasterizers such as WARP).
    /// Used by parity tests on machines without a real GPU.
    pub fn with_fallback_adapter() -> Result<Self, GpuError> {
        Self::acquire(true)
    }

    /// The adapter name, e.g. for `RenderStats` provenance.
    #[must_use]
    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    /// The wgpu backend (`Vulkan`, `Dx12`, ...), e.g. for provenance.
    #[must_use]
    pub fn backend(&self) -> &str {
        &self.backend
    }

    fn acquire(force_fallback: bool) -> Result<Self, GpuError> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: force_fallback,
        }))
        .map_err(|e| GpuError::new(format!("no wgpu adapter: {e}")))?;
        let info = adapter.get_info();
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("media-gpu-device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        }))
        .map_err(|e| GpuError::new(format!("request_device failed: {e}")))?;
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("media-gpu-composite"),
            source: wgpu::ShaderSource::Wgsl(FRAME_OPS_WGSL.into()),
        });
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("media-gpu-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("media-gpu-pl"),
            bind_group_layouts: &[&bind_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("media-gpu-pipeline"),
            layout: Some(&layout),
            module: &shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        Ok(Self {
            device,
            queue,
            pipeline,
            bind_layout,
            adapter_name: info.name,
            backend: format!("{:?}", info.backend),
        })
    }

    /// Paints the whole frame with one solid color (background contract).
    pub fn fill(&mut self, frame: &mut Frame, color: [u8; 4]) -> Result<(), GpuError> {
        let mut params = Params::op(OP_FILL, frame.width, frame.height);
        params.color = pack_rgba(color);
        frame.pixels = self.run(&params, &[], &[])?;
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
        let mut params = Params::op(OP_V_GRADIENT, frame.width, frame.height);
        params.stop_count = stops.len() as u32;
        params.segments = (stops.len() - 1) as f32;
        params.step = (frame.height.saturating_sub(1)).max(1) as f32;
        let packed: Vec<u32> = stops.iter().map(|stop| pack_rgba(*stop)).collect();
        frame.pixels = self.run(&params, &[], &packed)?;
        Ok(())
    }

    /// Radial gradient center→edge (background contract).
    pub fn radial(&mut self, frame: &mut Frame, center: [u8; 4], edge: [u8; 4]) -> Result<(), GpuError> {
        let mut params = Params::op(OP_RADIAL, frame.width, frame.height);
        params.center_color = pack_rgba(center);
        params.edge_color = pack_rgba(edge);
        frame.pixels = self.run(&params, &[], &[])?;
        Ok(())
    }

    /// Radial vignette: the one authored darkening formula
    /// (`media.post::Vignette`), alpha passthrough.
    pub fn vignette(&mut self, frame: &mut Frame, strength: f32) -> Result<(), GpuError> {
        let strength = strength.clamp(0.0, 1.0);
        if strength <= 0.0 {
            return Ok(());
        }
        let mut params = Params::op(OP_VIGNETTE, frame.width, frame.height);
        params.strength = strength;
        let source = std::mem::take(&mut frame.pixels);
        let out = self.run(&params, &source, &[]);
        frame.pixels = source;
        frame.pixels = out?;
        Ok(())
    }

    /// Camera resample: samples `src` around `focus` at `zoom` and optional
    /// rotation into `dst`, reproducing `Paint::resample_from` — including
    /// the truncating byte cast and the constant opaque alpha.
    pub fn resample(&mut self, dst: &mut Frame, src: &Frame, zoom: f32, focus: [f32; 2], rotate_deg: f32) -> Result<(), GpuError> {
        // The CPU reference clamps zoom, derives the tap count from the
        // shrink ratio and precomputes sin/cos; do the same so the shader
        // only runs the per-pixel arithmetic.
        let zoom = zoom.max(0.05);
        let angle = rotate_deg.to_radians();
        let (sin, cos) = angle.sin_cos();
        let ratio = src.width as f32 / (dst.width as f32 * zoom);
        let taps = (ratio.ceil() as u32).clamp(1, 3);
        let step = ratio / taps as f32;
        let mut params = Params::op(OP_RESAMPLE, dst.width, dst.height);
        params.src_width = src.width;
        params.src_height = src.height;
        params.taps = taps;
        params.zoom = zoom;
        params.focus_x = focus[0];
        params.focus_y = focus[1];
        params.step = step;
        params.sin = sin;
        params.cos = cos;
        dst.pixels = self.run(&params, &src.pixels, &[])?;
        Ok(())
    }

    /// Runs one operator over the whole output frame and returns its pixels.
    ///
    /// Dispatch is tiled to respect `max_compute_workgroups_per_dimension`;
    /// the shader walks the rest with a grid-stride loop.
    fn run(&self, params: &Params, src_pixels: &[u8], stops: &[u32]) -> Result<Vec<u8>, GpuError> {
        let out_len = (params.width * params.height * 4) as usize;
        let src_buf = self.storage_buffer(src_pixels, "media-gpu-src", wgpu::BufferUsages::STORAGE);
        let dst_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("media-gpu-dst"),
            size: (out_len.max(4)) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let stop_words: Vec<u8> = stops.iter().flat_map(|w| w.to_le_bytes()).collect();
        let stop_buf = self.storage_buffer(&stop_words, "media-gpu-stops", wgpu::BufferUsages::STORAGE);

        let max_wg = self.device.limits().max_compute_workgroups_per_dimension.max(1);
        let tiles_x = params.width.div_ceil(WORKGROUP).max(1);
        let tiles_y = params.height.div_ceil(WORKGROUP).max(1);
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("media-gpu-encode") });
        let mut tile = *params;
        let mut by = 0;
        while by < tiles_y {
            let ny = max_wg.min(tiles_y - by);
            let mut bx = 0;
            while bx < tiles_x {
                let nx = max_wg.min(tiles_x - bx);
                tile.tile_x = bx * WORKGROUP;
                tile.tile_y = by * WORKGROUP;
                let uniform = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("media-gpu-params"),
                    contents: &tile.to_bytes(),
                    usage: wgpu::BufferUsages::UNIFORM,
                });
                let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("media-gpu-bg"),
                    layout: &self.bind_layout,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
                        wgpu::BindGroupEntry { binding: 1, resource: src_buf.as_entire_binding() },
                        wgpu::BindGroupEntry { binding: 2, resource: dst_buf.as_entire_binding() },
                        wgpu::BindGroupEntry { binding: 3, resource: stop_buf.as_entire_binding() },
                    ],
                });
                {
                    let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                        label: Some("media-gpu-pass"),
                        timestamp_writes: None,
                    });
                    pass.set_pipeline(&self.pipeline);
                    pass.set_bind_group(0, &bind_group, &[]);
                    pass.dispatch_workgroups(nx, ny, 1);
                }
                bx += nx;
            }
            by += ny;
        }

        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("media-gpu-readback"),
            size: (out_len.max(4)) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_buffer_to_buffer(&dst_buf, 0, &staging, 0, out_len as u64);
        self.queue.submit(Some(encoder.finish()));

        let slice = staging.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        self.device.poll(wgpu::PollType::wait()).map_err(|e| GpuError::new(format!("readback poll failed: {e}")))?;
        rx.recv().map_err(|e| GpuError::new(format!("readback channel closed: {e}")))?
            .map_err(|e| GpuError::new(format!("readback map failed: {e}")))?;
        let view = slice.get_mapped_range();
        let out = view[..out_len].to_vec();
        drop(view);
        staging.unmap();
        Ok(out)
    }

    /// A storage buffer holding `bytes`, padded to the shader's word size.
    /// Zero-length inputs get a four-byte dummy so the binding is always
    /// valid (operators that read nothing still bind the slot).
    fn storage_buffer(&self, bytes: &[u8], label: &str, usage: wgpu::BufferUsages) -> wgpu::Buffer {
        let mut padded = bytes.to_vec();
        while padded.len() % 4 != 0 || padded.is_empty() {
            padded.push(0);
        }
        self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some(label),
            contents: &padded,
            usage,
        })
    }
}

/// The compositor without the backend compiled in: every acquisition fails,
/// so hosts fall back to the CPU path explicitly.
#[cfg(not(feature = "gpu"))]
pub struct GpuCompositor;

#[cfg(not(feature = "gpu"))]
impl GpuCompositor {
    pub fn new() -> Result<Self, GpuError> {
        Err(GpuError::new("media-gpu built without `gpu` feature"))
    }
    pub fn with_fallback_adapter() -> Result<Self, GpuError> {
        Self::new()
    }
    pub fn adapter_name(&self) -> &str {
        "none"
    }
    pub fn backend(&self) -> &str {
        "none"
    }
    pub fn fill(&mut self, _frame: &mut Frame, _color: [u8; 4]) -> Result<(), GpuError> {
        Err(GpuError::new("media-gpu built without `gpu` feature"))
    }
    pub fn v_gradient(&mut self, _frame: &mut Frame, _stops: &[[u8; 4]]) -> Result<(), GpuError> {
        Err(GpuError::new("media-gpu built without `gpu` feature"))
    }
    pub fn radial(&mut self, _frame: &mut Frame, _center: [u8; 4], _edge: [u8; 4]) -> Result<(), GpuError> {
        Err(GpuError::new("media-gpu built without `gpu` feature"))
    }
    pub fn vignette(&mut self, _frame: &mut Frame, _strength: f32) -> Result<(), GpuError> {
        Err(GpuError::new("media-gpu built without `gpu` feature"))
    }
    pub fn resample(&mut self, _dst: &mut Frame, _src: &Frame, _zoom: f32, _focus: [f32; 2], _rotate_deg: f32) -> Result<(), GpuError> {
        Err(GpuError::new("media-gpu built without `gpu` feature"))
    }
}

/// WGSL for the five display-space operators. The byte order, the loop order
/// and the cast rules mirror the CPU reference exactly; see the module docs.
#[cfg(feature = "gpu")]
const FRAME_OPS_WGSL: &str = r#"
struct Params {
    width: u32,
    height: u32,
    src_width: u32,
    src_height: u32,
    op: u32,
    taps: u32,
    stop_count: u32,
    zoom: f32,
    focus_x: f32,
    focus_y: f32,
    step: f32,
    sin: f32,
    cos: f32,
    strength: f32,
    segments: f32,
    color: u32,
    center_color: u32,
    edge_color: u32,
    tile_x: u32,
    tile_y: u32,
};

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var<storage, read> src: array<u32>;
@group(0) @binding(2) var<storage, read_write> dst: array<u32>;
@group(0) @binding(3) var<storage, read> stops: array<u32>;

fn ch(v: u32, i: u32) -> f32 {
    return f32((v >> (8u * i)) & 0xFFu);
}

fn mk(r: u32, g: u32, b: u32, a: u32) -> u32 {
    return r | (g << 8u) | (b << 16u) | (a << 24u);
}

// Truncating byte cast, as the resample reference writes `texel[k] as u8`.
fn to8_t(v: f32) -> u32 {
    return u32(clamp(v, 0.0, 255.0));
}

// Rounding byte cast, as `Rgba::lerp` and the vignette write it.
fn to8_r(v: f32) -> u32 {
    return u32(clamp(round(v), 0.0, 255.0));
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = clamp((x - e0) / (e1 - e0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

// Bilinear sample of the source frame in fractional pixel coordinates,
// channels as display bytes 0..255 (no normalization), same order as
// `media_image::Frame::sample`.
fn sample(x_in: f32, y_in: f32) -> vec4<f32> {
    let wm = f32(max(p.src_width - 1u, 0u));
    let hm = f32(max(p.src_height - 1u, 0u));
    let x = clamp(x_in, 0.0, wm);
    let y = clamp(y_in, 0.0, hm);
    let x0f = floor(x);
    let y0f = floor(y);
    let fx = x - x0f;
    let fy = y - y0f;
    let x0 = u32(x0f);
    let y0 = u32(y0f);
    let x1 = min(x0 + 1u, max(p.src_width - 1u, 0u));
    let y1 = min(y0 + 1u, max(p.src_height - 1u, 0u));
    let sw = p.src_width;
    let c00 = src[y0 * sw + x0];
    let c10 = src[y0 * sw + x1];
    let c01 = src[y1 * sw + x0];
    let c11 = src[y1 * sw + x1];
    let top0 = vec4<f32>(ch(c00, 0u), ch(c00, 1u), ch(c00, 2u), ch(c00, 3u));
    let top1 = vec4<f32>(ch(c10, 0u), ch(c10, 1u), ch(c10, 2u), ch(c10, 3u));
    let bot0 = vec4<f32>(ch(c01, 0u), ch(c01, 1u), ch(c01, 2u), ch(c01, 3u));
    let bot1 = vec4<f32>(ch(c11, 0u), ch(c11, 1u), ch(c11, 2u), ch(c11, 3u));
    let top = top0 + (top1 - top0) * fx;
    let bottom = bot0 + (bot1 - bot0) * fx;
    return top + (bottom - top) * fy;
}

// `Rgba::lerp`: clamp t, mix every channel, rounding cast.
fn lerp_color(a: u32, b: u32, t_in: f32) -> u32 {
    let t = clamp(t_in, 0.0, 1.0);
    let r = to8_r(ch(a, 0u) + (ch(b, 0u) - ch(a, 0u)) * t);
    let g = to8_r(ch(a, 1u) + (ch(b, 1u) - ch(a, 1u)) * t);
    let bb = to8_r(ch(a, 2u) + (ch(b, 2u) - ch(a, 2u)) * t);
    let aa = to8_r(ch(a, 3u) + (ch(b, 3u) - ch(a, 3u)) * t);
    return mk(r, g, bb, aa);
}

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x = gid.x + p.tile_x;
    let y = gid.y + p.tile_y;
    if (x >= p.width || y >= p.height) {
        return;
    }
    let i = y * p.width + x;
    switch p.op {
        case 0u: { // resample
            let w = f32(p.width);
            let h = f32(p.height);
            let v = (f32(y) + 0.5) / h - 0.5;
            let u = (f32(x) + 0.5) / w - 0.5;
            let zu = u / p.zoom;
            let zv = v / p.zoom;
            let ru = zu * p.cos + zv * p.sin;
            let rv = -zu * p.sin + zv * p.cos;
            let sx = (p.focus_x + ru) * f32(p.src_width) - 0.5;
            let sy = (p.focus_y + rv) * f32(p.src_height) - 0.5;
            var t: vec4<f32>;
            if (p.taps == 1u) {
                t = sample(sx, sy);
            } else {
                let half = (f32(p.taps) - 1.0) / 2.0;
                var acc = vec4<f32>(0.0, 0.0, 0.0, 0.0);
                for (var ty: u32 = 0u; ty < p.taps; ty = ty + 1u) {
                    for (var tx: u32 = 0u; tx < p.taps; tx = tx + 1u) {
                        acc = acc + sample(sx + (f32(tx) - half) * p.step, sy + (f32(ty) - half) * p.step);
                    }
                }
                t = acc / f32(p.taps * p.taps);
            }
            dst[i] = mk(to8_t(t.r), to8_t(t.g), to8_t(t.b), 255u);
        }
        case 1u: { // vignette
            let u = (f32(x) + 0.5) / f32(p.width);
            let v = (f32(y) + 0.5) / f32(p.height);
            let du = u - 0.5;
            let dv = v - 0.5;
            let d = sqrt(du * du + dv * dv) * 1.4142135623730951;
            let edge = smoothstep(0.25, 0.95, d);
            let factor = 1.0 - p.strength * edge;
            let px = src[i];
            let r = to8_r(ch(px, 0u) * factor);
            let g = to8_r(ch(px, 1u) * factor);
            let b = to8_r(ch(px, 2u) * factor);
            dst[i] = mk(r, g, b, (px >> 24u) & 0xFFu);
        }
        case 2u: { // fill
            dst[i] = p.color;
        }
        case 3u: { // v_gradient
            let t = (f32(y) / p.step) * p.segments;
            let seg = min(u32(t), p.stop_count - 2u);
            let local = t - f32(seg);
            dst[i] = lerp_color(stops[seg], stops[seg + 1u], local);
        }
        case 4u: { // radial
            let cx = f32(p.width) / 2.0;
            let cy = f32(p.height) / 2.0;
            let max_r = sqrt(cx * cx + cy * cy);
            let dx = f32(x) - cx;
            let dy = f32(y) - cy;
            let t = sqrt(dx * dx + dy * dy) / max_r;
            dst[i] = lerp_color(p.center_color, p.edge_color, t);
        }
        default: {}
    }
}
"#;
