use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use hardware_capability::{Feature, FeatureSet, WiredStrategies};
use hardware_emitter::{emit_program_spirv, emit_raster_tri_spirv, emit_spirv, program_from_task};
use hardware_ir::{Graph, TaskKind, TaskNode};
use hardware_program::{Program, program_hash};
use hardware_scheduler::{Backend, Error as SchedulerError, Result};
use hardware_types::{Dispatch, ParamValue, ResourceId};

use crate::{
    driver::{VulkanDriver, ffi::*},
    params::{BackendError, f32_param, resource_param, str_param, u32_param},
};

struct DeviceBuffer {
    buffer: VkBuffer,
    memory: VkDeviceMemory,
    bytes: usize,
}

struct ComputePipelineCache {
    spirv: Vec<u8>,
    entry: String,
    buffer_count: usize,
    push_size: usize,
    module: VkShaderModule,
    set_layout: VkDescriptorSetLayout,
    pipeline_layout: VkPipelineLayout,
    pipeline: VkPipeline,
}

struct LoweredProgram {
    float64: bool,
    max_resource: Option<u32>,
    spirv: Vec<u8>,
}

struct ProgramCacheEntry {
    version: u32,
    hash: u64,
    body: String,
    lowered: Arc<LoweredProgram>,
}

/// 通用程序解析与 lowering 缓存统计，不代表任务已经成功执行。
#[derive(Debug, Clone, Copy, Default)]
pub struct ProgramCacheStats {
    /// 正文、版本与哈希完全匹配的缓存命中次数。
    pub hits: u64,
    /// 必须重新解析及验证的请求次数，包含失败请求。
    pub misses: u64,
    /// 成功完成的通用程序 lowering 次数。
    pub lowerings: u64,
    /// 成功 lowering 的累计主机耗时，不包含 GPU 执行。
    pub lowering_time: Duration,
    /// 当前缓存中的程序数量，最多十六个。
    pub entries: usize,
}

/// Vulkan compute / graphics backend with device-resident buffers.
pub struct VulkanBackend {
    driver: VulkanDriver,
    command_pool: VkCommandPool,
    graphics: bool,
    supports_float64: bool,
    buffers: BTreeMap<ResourceId, DeviceBuffer>,
    compute_pipelines: Vec<ComputePipelineCache>,
    program_cache: Vec<ProgramCacheEntry>,
    program_cache_stats: ProgramCacheStats,
    strategies: WiredStrategies,
    /// Last descriptor path label taken by a submit (for thin-gate assertions).
    last_descriptor_path: &'static str,
    /// Last sync path label taken by a submit (for thin-gate assertions).
    last_sync_path: &'static str,
    /// Last pipeline path label taken by a submit (for thin-gate assertions).
    last_pipeline_path: &'static str,
}

impl VulkanBackend {
    /// Open the first physical device with a compute-capable queue.
    pub fn new() -> Result<Self> {
        let driver = VulkanDriver::new().map_err(|error| map_backend_err(BackendError(error.to_string())))?;
        let command_pool = driver
            .create_command_pool(VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT)
            .map_err(|error| map_backend_err(BackendError(format!("command_pool: {error}"))))?;
        let graphics = driver.graphics();
        let supports_float64 = driver.supports_float64();

        let features = FeatureSet::new()
            .with(Feature::ProgramFloat32)
            .with(Feature::Bindless)
            .with(Feature::DescriptorBuffer)
            .with(Feature::DynamicRendering)
            .with(Feature::Synchronization2);
        let strategies = WiredStrategies::from_features(&features);

        Ok(Self {
            driver,
            command_pool,
            graphics,
            supports_float64,
            buffers: BTreeMap::new(),
            compute_pipelines: Vec::new(),
            program_cache: Vec::new(),
            program_cache_stats: ProgramCacheStats::default(),
            strategies,
            last_descriptor_path: "",
            last_sync_path: "",
            last_pipeline_path: "",
        })
    }

    /// Degrade-chain strategies resolved when the backend opened.
    pub fn strategies(&self) -> WiredStrategies {
        self.strategies
    }

    /// 返回程序编译缓存统计，不能代替执行与 readback 证明。
    pub fn program_cache_stats(&self) -> ProgramCacheStats {
        ProgramCacheStats { entries: self.program_cache.len(), ..self.program_cache_stats }
    }

    /// Descriptor path label from the last submit that bound descriptors.
    pub fn last_descriptor_path(&self) -> &'static str {
        self.last_descriptor_path
    }

    /// Sync path label from the last submit that issued a barrier.
    pub fn last_sync_path(&self) -> &'static str {
        self.last_sync_path
    }

    /// Pipeline path label from the last submit that created a pipeline.
    pub fn last_pipeline_path(&self) -> &'static str {
        self.last_pipeline_path
    }

    fn mark_sync_path(&mut self) {
        self.last_sync_path = self.strategies.sync_label();
    }

    fn mark_descriptor_path(&mut self) {
        self.last_descriptor_path = self.strategies.descriptor_label();
    }

    fn mark_pipeline_path(&mut self) {
        self.last_pipeline_path = self.strategies.pipeline_label();
    }

    /// Upload host `f32` values into an allocated device buffer.
    pub fn write_f32(&mut self, id: ResourceId, values: &[f32]) -> Result<()> {
        let bytes = values.len() * 4;
        let buf = self.buffers.get(&id).ok_or_else(|| map_backend_err(BackendError(format!("resource {} not allocated", id.0))))?;
        if buf.bytes != bytes {
            return Err(map_backend_err(BackendError(format!("resource {} expected {} bytes, got {bytes}", id.0, buf.bytes))));
        }
        let mut host = Vec::with_capacity(bytes);
        for v in values {
            host.extend_from_slice(&v.to_le_bytes());
        }
        self.upload(id, &host)
    }

    /// Download a device buffer as `f32` values.
    pub fn read_f32(&mut self, id: ResourceId) -> Result<Vec<f32>> {
        let bytes = self.buffers.get(&id).ok_or_else(|| map_backend_err(BackendError(format!("resource {} not allocated", id.0))))?.bytes;
        let host = self.readback(id, bytes)?;
        Ok(host.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
    }

    /// 上传同质 F64 资源，尺寸必须精确匹配，不截断为 F32。
    pub fn write_f64(&mut self, id: ResourceId, values: &[f64]) -> Result<()> {
        let host = values.iter().flat_map(|value| value.to_le_bytes()).collect::<Vec<_>>();
        expect_bytes(id, self.buf_bytes(id)?, host.len())?;
        self.upload(id, &host)
    }

    /// 读回同质 F64 资源，拒绝不完整的八字节元素。
    pub fn read_f64(&mut self, id: ResourceId) -> Result<Vec<f64>> {
        let bytes = self.buf_bytes(id)?;
        if bytes % 8 != 0 {
            return Err(map_backend_err(BackendError(format!("resource {} is not an F64 buffer", id.0))));
        }
        let host = self.readback(id, bytes)?;
        Ok(host.chunks_exact(8).map(|value| f64::from_le_bytes(value.try_into().unwrap())).collect())
    }

    /// Download a device buffer as raw bytes (e.g. `R8G8B8A8` raster readback).
    pub fn read_u8(&mut self, id: ResourceId) -> Result<Vec<u8>> {
        let bytes = self.buffers.get(&id).ok_or_else(|| map_backend_err(BackendError(format!("resource {} not allocated", id.0))))?.bytes;
        self.readback(id, bytes)
    }

    /// Run every task on this backend. Caller must [`prepare`] and seed inputs first.
    pub fn run_prepared(&mut self, graph: &Graph) -> Result<()> {
        graph.validate().map_err(SchedulerError::from)?;
        let order = graph.tasks.topological_order().map_err(SchedulerError::from)?;
        for task_id in order {
            let task = graph.tasks.nodes.iter().find(|t| t.id == task_id).expect("task in order must exist");
            self.submit_task(graph, task)?;
        }
        Ok(())
    }

    fn upload(&mut self, id: ResourceId, bytes: &[u8]) -> Result<()> {
        let (staging, staging_mem) = self
            .driver
            .create_buffer(
                bytes.len(),
                VK_BUFFER_USAGE_TRANSFER_SRC_BIT,
                VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
            )
            .map_err(vk_err)?;
        {
            let ptr = self.driver.map_memory(staging_mem, bytes.len()).map_err(vk_err)?;
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast(), bytes.len());
            }
            self.driver.unmap_memory(staging_mem);
        }
        let dst = self.buffers.get(&id).unwrap().buffer;
        self.with_commands(|cmd| {
            self.driver.cmd_copy_buffer(cmd, staging, dst, bytes.len());
        })?;
        self.driver.destroy_buffer(staging, staging_mem);
        Ok(())
    }

    fn readback(&mut self, id: ResourceId, bytes: usize) -> Result<Vec<u8>> {
        let (staging, staging_mem) = self
            .driver
            .create_buffer(bytes, VK_BUFFER_USAGE_TRANSFER_DST_BIT, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)
            .map_err(vk_err)?;
        let src = self.buffers.get(&id).unwrap().buffer;
        self.with_commands(|cmd| {
            self.driver.cmd_copy_buffer(cmd, src, staging, bytes);
        })?;
        let mut out = vec![0u8; bytes];
        let ptr = self.driver.map_memory(staging_mem, bytes).map_err(vk_err)?;
        unsafe {
            std::ptr::copy_nonoverlapping(ptr.cast(), out.as_mut_ptr(), bytes);
        }
        self.driver.unmap_memory(staging_mem);
        self.driver.destroy_buffer(staging, staging_mem);
        Ok(out)
    }

    fn create_buffer(&self, bytes: usize, usage: u32, props: u32) -> Result<(VkBuffer, VkDeviceMemory)> {
        self.driver.create_buffer(bytes, usage, props).map_err(vk_err)
    }

    fn create_image(&self, width: u32, height: u32, usage: u32, props: u32) -> Result<(VkImage, VkDeviceMemory)> {
        self.driver.create_image(width, height, usage, props).map_err(vk_err)
    }

    fn with_commands(&self, record: impl FnOnce(VkCommandBuffer)) -> Result<()> {
        let cmd = self.driver.allocate_command_buffer(self.command_pool).map_err(vk_err)?;
        self.driver.begin_command_buffer(cmd).map_err(vk_err)?;
        record(cmd);
        self.driver.end_command_buffer(cmd).map_err(vk_err)?;
        let submitted = self.driver.submit_and_wait(cmd);
        self.driver.free_command_buffer(self.command_pool, cmd);
        submitted.map_err(vk_err)
    }

    fn run_copy(&mut self, task: &TaskNode) -> Result<()> {
        let src = resource_param(task, "src").map_err(map_backend_err)?;
        let dst = resource_param(task, "dst").map_err(map_backend_err)?;
        let src_bytes = self.buf_bytes(src)?;
        let dst_bytes = self.buf_bytes(dst)?;
        if src_bytes != dst_bytes {
            return Err(map_backend_err(BackendError(format!("copy size mismatch: src {src_bytes} dst {dst_bytes}"))));
        }
        let src_buf = self.buffers.get(&src).unwrap().buffer;
        let dst_buf = self.buffers.get(&dst).unwrap().buffer;
        self.with_commands(|cmd| {
            self.driver.cmd_copy_buffer(cmd, src_buf, dst_buf, src_bytes);
        })
    }

    fn run_fill(&mut self, task: &TaskNode) -> Result<()> {
        let dst = resource_param(task, "dst").map_err(map_backend_err)?;
        let _value = f32_param(task, "value").map_err(map_backend_err)?;
        let bytes = self.buf_bytes(dst)?;
        if bytes % 4 != 0 {
            return Err(map_backend_err(BackendError(format!("fill destination {} length {bytes} is not a multiple of 4", dst.0))));
        }
        let count = (bytes / 4) as u32;
        let program = program_from_task(task).map_err(|err| map_emit_err(err.to_string()))?;
        let spirv = emit_spirv(&program).map_err(map_emit_err)?;
        let dst_buf = self.buffers.get(&dst).unwrap().buffer;
        self.dispatch_compute(&spirv, &program.entry, &[dst_buf], &count.to_ne_bytes(), count.div_ceil(64).max(1))
    }

    fn run_matmul(&mut self, task: &TaskNode) -> Result<()> {
        let a = resource_param(task, "a").map_err(map_backend_err)?;
        let b = resource_param(task, "b").map_err(map_backend_err)?;
        let out = resource_param(task, "out").map_err(map_backend_err)?;
        let m = u32_param(task, "m").map_err(map_backend_err)?;
        let n = u32_param(task, "n").map_err(map_backend_err)?;
        let k = u32_param(task, "k").map_err(map_backend_err)?;
        expect_bytes(a, self.buf_bytes(a)?, (m as usize) * (k as usize) * 4)?;
        expect_bytes(b, self.buf_bytes(b)?, (k as usize) * (n as usize) * 4)?;
        expect_bytes(out, self.buf_bytes(out)?, (m as usize) * (n as usize) * 4)?;

        let program = program_from_task(task).map_err(|err| map_emit_err(err.to_string()))?;
        let spirv = emit_spirv(&program).map_err(map_emit_err)?;
        let a_buf = self.buffers.get(&a).unwrap().buffer;
        let b_buf = self.buffers.get(&b).unwrap().buffer;
        let out_buf = self.buffers.get(&out).unwrap().buffer;
        let mut push = [0u8; 16];
        push[0..4].copy_from_slice(&m.to_ne_bytes());
        push[4..8].copy_from_slice(&n.to_ne_bytes());
        push[8..12].copy_from_slice(&k.to_ne_bytes());
        self.dispatch_compute(&spirv, &program.entry, &[a_buf, b_buf, out_buf], &push, (m * n).div_ceil(64).max(1))
    }

    fn lowered_program(&mut self, task: &TaskNode) -> Result<Arc<LoweredProgram>> {
        let (version, expected_hash, body) = match task.params.get("program") {
            Some(ParamValue::Program { version, hash, body }) => (*version, *hash, body),
            _ => return Err(map_backend_err(BackendError("Dispatch requires a serialized generic UCF program".into()))),
        };
        if version != 1 {
            return Err(map_backend_err(BackendError(format!("unsupported generic UCF program version {version}"))));
        }
        if let Some(index) =
            self.program_cache.iter().position(|entry| entry.version == version && entry.hash == expected_hash && entry.body == *body)
        {
            self.program_cache_stats.hits += 1;
            let entry = self.program_cache.remove(index);
            let lowered = Arc::clone(&entry.lowered);
            self.program_cache.push(entry);
            return Ok(lowered);
        }
        self.program_cache_stats.misses += 1;
        let program: Program =
            serde_json::from_str(body).map_err(|e| map_backend_err(BackendError(format!("invalid generic UCF program body: {e}"))))?;
        let actual_hash = program_hash(&program);
        if actual_hash != expected_hash {
            return Err(map_backend_err(BackendError(format!(
                "generic UCF program hash mismatch: expected {expected_hash}, got {actual_hash}"
            ))));
        }
        let float64 = program.entry_fn().is_some_and(|entry| entry.ret == hardware_program::Ty::f64());
        if float64 && !self.supports_float64 {
            return Err(map_backend_err(BackendError("Vulkan device does not support ProgramFloat64".into())));
        }
        let max_resource = program
            .functions
            .iter()
            .flat_map(|function| function.body.iter())
            .map(hardware_program_resource_slots)
            .collect::<std::result::Result<Vec<_>, BackendError>>()
            .map_err(map_backend_err)?
            .into_iter()
            .flatten()
            .max();
        let started = Instant::now();
        let spirv = emit_program_spirv(&program, "hardware_program_main").map_err(|e| map_emit_err(e.to_string()))?;
        self.program_cache_stats.lowering_time += started.elapsed();
        self.program_cache_stats.lowerings += 1;
        let lowered = Arc::new(LoweredProgram { float64, max_resource, spirv });
        if self.program_cache.len() == 16 {
            self.program_cache.remove(0);
        }
        self.program_cache.push(ProgramCacheEntry { version, hash: expected_hash, body: body.clone(), lowered: Arc::clone(&lowered) });
        Ok(lowered)
    }

    fn run_program_task(&mut self, task: &TaskNode) -> Result<()> {
        let lowered = self.lowered_program(task)?;
        let output = resource_param(task, "output").map_err(map_backend_err)?;
        let float64 = lowered.float64;
        let scalar_bytes = if float64 { 8 } else { 4 };
        let bytes = self.buf_bytes(output)?;
        if bytes == 0 || bytes % scalar_bytes != 0 {
            return Err(map_backend_err(BackendError(format!(
                "generic UCF output resource {} must contain {scalar_bytes}-byte scalar elements",
                output.0
            ))));
        }
        let output_count = u32::try_from(bytes / scalar_bytes)
            .map_err(|_| map_backend_err(BackendError("generic UCF output element count exceeds u32".into())))?;
        let dispatch_count = match task.dispatch {
            Dispatch::Explicit { x, y, z } => x
                .checked_mul(y)
                .and_then(|value| value.checked_mul(z))
                .ok_or_else(|| map_backend_err(BackendError("generic UCF dispatch dimensions overflow".into())))?,
            Dispatch::Auto => return Err(map_backend_err(BackendError("generic UCF dispatch requires explicit dimensions".into()))),
        };
        if dispatch_count > output_count {
            return Err(map_backend_err(BackendError(format!(
                "generic UCF dispatch count {dispatch_count} exceeds output capacity {output_count}"
            ))));
        }
        let mut dispatch_buffers = vec![self.buffers.get(&output).unwrap().buffer];
        if let Some(max_resource) = lowered.max_resource {
            for slot in 0..=max_resource {
                let key = format!("buffer_{slot}");
                let resource = match task.params.get(&key) {
                    Some(ParamValue::I64(value)) if *value >= 0 => ResourceId(*value as u64),
                    Some(_) => return Err(map_backend_err(BackendError(format!("{key} must be a non-negative resource id")))),
                    None => return Err(map_backend_err(BackendError(format!("generic UCF program requires {key}")))),
                };
                let buffer = self
                    .buffers
                    .get(&resource)
                    .ok_or_else(|| map_backend_err(BackendError(format!("generic UCF input resource {} is not allocated", resource.0))))?;
                if buffer.bytes % scalar_bytes != 0 {
                    return Err(map_backend_err(BackendError(format!(
                        "generic UCF resource {} must contain {scalar_bytes}-byte scalar elements",
                        resource.0
                    ))));
                }
                dispatch_buffers.push(buffer.buffer);
            }
        }
        let mut push = vec![0u8; if float64 { 128 } else { 68 }];
        push[0..4].copy_from_slice(&dispatch_count.to_ne_bytes());
        for slot in 0..16u32 {
            if let Some(value) = task.params.get(&format!("uniform_{slot}")) {
                let value = match value {
                    ParamValue::F64(value) => *value,
                    ParamValue::I64(value) => *value as f64,
                    _ => return Err(map_backend_err(BackendError(format!("uniform_{slot} must be numeric")))),
                };
                let offset = scalar_bytes * (slot as usize + 1);
                if float64 {
                    if slot >= 15 {
                        return Err(map_backend_err(BackendError("F64 push constants support at most fifteen uniforms".into())));
                    }
                    push[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
                }
                else {
                    push[offset..offset + 4].copy_from_slice(&(value as f32).to_ne_bytes());
                }
            }
        }
        self.dispatch_compute(&lowered.spirv, "hardware_program_main", &dispatch_buffers, &push, dispatch_count.div_ceil(64).max(1))
    }

    /// Rasterize into an R8G8B8A8 image and copy packed pixels into buffer `dst`.
    ///
    /// Params: `dst`, `width`, `height`, optional `r`/`g`/`b`/`a` in `0..1`,
    /// optional `draw` = `clear` (default) or `tri` (yellow NDC triangle after clear).
    fn run_raster(&mut self, task: &TaskNode) -> Result<()> {
        let dst = resource_param(task, "dst").map_err(map_backend_err)?;
        let width = u32_param(task, "width").map_err(map_backend_err)?;
        let height = u32_param(task, "height").map_err(map_backend_err)?;
        if width == 0 || height == 0 {
            return Err(map_backend_err(BackendError("raster width/height must be > 0".into())));
        }
        let draw = str_param(task, "draw").unwrap_or("clear");
        if draw != "clear" && draw != "tri" {
            return Err(map_backend_err(BackendError(format!("raster draw must be `clear` or `tri`, got `{draw}`"))));
        }
        if draw == "tri" && !self.graphics {
            return Err(map_backend_err(BackendError("raster draw=tri requires a graphics queue".into())));
        }
        let r = f32_param(task, "r").unwrap_or(0.0);
        let g = f32_param(task, "g").unwrap_or(0.0);
        let b = f32_param(task, "b").unwrap_or(0.0);
        let a = f32_param(task, "a").unwrap_or(1.0);
        let need = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| map_backend_err(BackendError("raster size overflow".into())))?;
        let dst_bytes = self.buf_bytes(dst)?;
        if dst_bytes < need {
            return Err(map_backend_err(BackendError(format!("raster destination {} has {dst_bytes} bytes, need at least {need}", dst.0))));
        }

        if draw == "tri" {
            self.run_raster_tri(dst, width, height, need, [r, g, b, a])
        }
        else {
            self.run_raster_clear(dst, width, height, need, [r, g, b, a])
        }
    }

    fn run_raster_clear(&mut self, dst: ResourceId, width: u32, height: u32, need: usize, color: [f32; 4]) -> Result<()> {
        let (image, image_mem) = self.create_image(
            width,
            height,
            VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT,
            VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT,
        )?;
        let (staging, staging_mem) = self.create_buffer(
            need,
            VK_BUFFER_USAGE_TRANSFER_DST_BIT,
            VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
        )?;

        self.mark_sync_path();
        self.with_commands(|cmd| {
            self.driver.cmd_image_barrier(
                cmd,
                image,
                VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                VK_PIPELINE_STAGE_TRANSFER_BIT,
                0,
                VK_ACCESS_TRANSFER_WRITE_BIT,
                VK_IMAGE_LAYOUT_UNDEFINED,
                VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
            );
            self.driver.cmd_clear_color(cmd, image, color);
            self.driver.cmd_image_barrier(
                cmd,
                image,
                VK_PIPELINE_STAGE_TRANSFER_BIT,
                VK_PIPELINE_STAGE_TRANSFER_BIT,
                VK_ACCESS_TRANSFER_WRITE_BIT,
                VK_ACCESS_TRANSFER_READ_BIT,
                VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
            );
            self.driver.cmd_copy_image_to_buffer(cmd, image, staging, width, height);
        })?;

        let pixels = self.map_staging_pixels(staging_mem, need)?;
        self.driver.destroy_buffer(staging, staging_mem);
        self.driver.destroy_image(image, image_mem);
        self.upload(dst, &pixels)
    }

    fn run_raster_tri(&mut self, dst: ResourceId, width: u32, height: u32, need: usize, clear_color: [f32; 4]) -> Result<()> {
        let spirv = emit_raster_tri_spirv().map_err(map_emit_err)?;
        let (image, image_mem) = self.create_image(
            width,
            height,
            VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT,
            VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT,
        )?;
        let view = self.driver.create_image_view(image).map_err(vk_err)?;
        let render_pass = self.driver.create_color_render_pass(VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL).map_err(vk_err)?;
        let framebuffer = self.driver.create_framebuffer(render_pass, view, width, height).map_err(vk_err)?;

        let words: Vec<u32> = spirv.module.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        let module = self.driver.create_shader_module(&words).map_err(vk_err)?;
        let pipeline_layout =
            self.driver.create_pipeline_layout(None, 0, VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT).map_err(vk_err)?;
        let (vs_name, fs_name) = (std::ffi::CString::new("vs_main").unwrap(), std::ffi::CString::new("fs_main").unwrap());
        self.mark_pipeline_path();
        let pipeline = self
            .driver
            .create_triangle_pipeline(module, &vs_name, module, &fs_name, pipeline_layout, render_pass, width, height)
            .map_err(vk_err)?;

        let (staging, staging_mem) = self.create_buffer(
            need,
            VK_BUFFER_USAGE_TRANSFER_DST_BIT,
            VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
        )?;

        self.mark_sync_path();
        self.with_commands(|cmd| {
            self.driver.cmd_draw_triangle(cmd, render_pass, framebuffer, width, height, clear_color);
            self.driver.cmd_bind_pipeline(cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, pipeline);
            self.driver.cmd_draw(cmd);
            self.driver.cmd_end_render_pass(cmd);

            self.driver.cmd_image_barrier(
                cmd,
                image,
                VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                VK_PIPELINE_STAGE_TRANSFER_BIT,
                VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
                VK_ACCESS_TRANSFER_READ_BIT,
                VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
            );
            self.driver.cmd_copy_image_to_buffer(cmd, image, staging, width, height);
        })?;

        let pixels = self.map_staging_pixels(staging_mem, need)?;

        self.driver.destroy_pipeline(pipeline);
        self.driver.destroy_pipeline_layout(pipeline_layout);
        self.driver.destroy_shader_module(module);
        self.driver.destroy_framebuffer(framebuffer);
        self.driver.destroy_render_pass(render_pass);
        self.driver.destroy_image_view(view);
        self.driver.destroy_buffer(staging, staging_mem);
        self.driver.destroy_image(image, image_mem);

        self.upload(dst, &pixels)
    }

    fn map_staging_pixels(&self, staging_mem: VkDeviceMemory, need: usize) -> Result<Vec<u8>> {
        let pointer = self.driver.map_memory(staging_mem, need).map_err(vk_err)?;
        let mut pixels = vec![0u8; need];
        unsafe {
            std::ptr::copy_nonoverlapping(pointer.cast(), pixels.as_mut_ptr(), need);
        }
        self.driver.unmap_memory(staging_mem);
        Ok(pixels)
    }

    fn dispatch_compute(&mut self, spirv_bytes: &[u8], entry: &str, buffers: &[VkBuffer], push_bytes: &[u8], groups_x: u32) -> Result<()> {
        let cache_index = if let Some(index) = self.compute_pipelines.iter().position(|cached| {
            cached.spirv == spirv_bytes && cached.entry == entry && cached.buffer_count == buffers.len() && cached.push_size == push_bytes.len()
        }) {
            index
        }
        else {
            let words: Vec<u32> = spirv_bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
            let module = self.driver.create_shader_module(&words).map_err(vk_err)?;
            self.mark_descriptor_path();
            let set_layout = self.driver.create_storage_set_layout(buffers.len() as u32, VK_SHADER_STAGE_COMPUTE_BIT).map_err(vk_err)?;
            let pipeline_layout =
                self.driver.create_pipeline_layout(Some(set_layout), push_bytes.len() as u32, VK_SHADER_STAGE_COMPUTE_BIT).map_err(vk_err)?;
            let entry_c = std::ffi::CString::new(entry).map_err(|e| map_backend_err(BackendError(format!("invalid entry `{entry}`: {e}"))))?;
            self.mark_pipeline_path();
            let pipeline = self.driver.create_compute_pipeline(module, &entry_c, pipeline_layout).map_err(vk_err)?;
            self.compute_pipelines.push(ComputePipelineCache {
                spirv: spirv_bytes.to_vec(),
                entry: entry.to_owned(),
                buffer_count: buffers.len(),
                push_size: push_bytes.len(),
                module,
                set_layout,
                pipeline_layout,
                pipeline,
            });
            self.compute_pipelines.len() - 1
        };
        let (set_layout, pipeline_layout, pipeline) = {
            let cached = &self.compute_pipelines[cache_index];
            (cached.set_layout, cached.pipeline_layout, cached.pipeline)
        };

        let desc_pool = self.driver.create_descriptor_pool(buffers.len() as u32).map_err(vk_err)?;
        let set = self.driver.allocate_descriptor_set(desc_pool, set_layout).map_err(vk_err)?;
        self.driver.update_storage_buffers(set, buffers);

        self.with_commands(|cmd| {
            self.driver.cmd_memory_barrier(
                cmd,
                VK_PIPELINE_STAGE_ALL_COMMANDS_BIT,
                VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                VK_ACCESS_MEMORY_WRITE_BIT,
                VK_ACCESS_SHADER_READ_BIT | VK_ACCESS_SHADER_WRITE_BIT,
            );
            self.driver.cmd_bind_pipeline(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pipeline);
            self.driver.cmd_bind_descriptor_set(cmd, VK_PIPELINE_BIND_POINT_COMPUTE, pipeline_layout, set);
            if !push_bytes.is_empty() {
                self.driver.cmd_push_constants(cmd, pipeline_layout, VK_SHADER_STAGE_COMPUTE_BIT, push_bytes);
            }
            self.driver.cmd_dispatch(cmd, groups_x);
            self.driver.cmd_memory_barrier(
                cmd,
                VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
                VK_PIPELINE_STAGE_TRANSFER_BIT,
                VK_ACCESS_SHADER_WRITE_BIT,
                VK_ACCESS_TRANSFER_READ_BIT,
            );
        })?;

        self.driver.destroy_descriptor_pool(desc_pool);
        Ok(())
    }

    fn buf_bytes(&self, id: ResourceId) -> Result<usize> {
        self.buffers.get(&id).map(|b| b.bytes).ok_or_else(|| map_backend_err(BackendError(format!("resource {} not allocated", id.0))))
    }
}

fn hardware_program_resource_slots(stmt: &hardware_program::Stmt) -> std::result::Result<Vec<u32>, BackendError> {
    fn expr_slots(expr: &hardware_program::Expr, slots: &mut Vec<u32>) {
        match expr {
            hardware_program::Expr::LoadBuffer { resource, index } => {
                slots.push(*resource);
                expr_slots(index, slots);
            }
            hardware_program::Expr::BinOp { lhs, rhs, .. } => {
                expr_slots(lhs, slots);
                expr_slots(rhs, slots);
            }
            hardware_program::Expr::UnOp { arg, .. } => expr_slots(arg, slots),
            hardware_program::Expr::Select { cond, then_, else_ } => {
                expr_slots(cond, slots);
                expr_slots(then_, slots);
                expr_slots(else_, slots);
            }
            hardware_program::Expr::Vec3Compose { x, y, z } => {
                expr_slots(x, slots);
                expr_slots(y, slots);
                expr_slots(z, slots);
            }
            hardware_program::Expr::Call { args, .. } => {
                for arg in args {
                    expr_slots(arg, slots);
                }
            }
            _ => {}
        }
    }
    let mut slots = Vec::new();
    match stmt {
        hardware_program::Stmt::Let { init, .. }
        | hardware_program::Stmt::StoreLocal { value: init, .. }
        | hardware_program::Stmt::Return(init)
        | hardware_program::Stmt::Expr(init) => expr_slots(init, &mut slots),
        hardware_program::Stmt::StoreBuffer { resource, index, value } => {
            slots.push(*resource);
            expr_slots(index, &mut slots);
            expr_slots(value, &mut slots);
        }
        hardware_program::Stmt::If { cond, then_body, else_body } => {
            expr_slots(cond, &mut slots);
            for nested in then_body.iter().chain(else_body) {
                slots.extend(hardware_program_resource_slots(nested)?);
            }
        }
        hardware_program::Stmt::For { body, .. } | hardware_program::Stmt::While { body, .. } => {
            for nested in body {
                slots.extend(hardware_program_resource_slots(nested)?);
            }
        }
        hardware_program::Stmt::Break | hardware_program::Stmt::Continue => {}
        hardware_program::Stmt::Cfg { blocks, .. } => {
            for block in blocks {
                for instruction in &block.instructions {
                    match instruction {
                        hardware_program::CfgInstruction::LoadBuffer { resource, .. }
                        | hardware_program::CfgInstruction::StoreBuffer { resource, .. } => slots.push(*resource),
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(slots)
}

impl Drop for VulkanBackend {
    fn drop(&mut self) {
        let _ = self.driver.device_wait_idle();
        for cached in self.compute_pipelines.drain(..) {
            self.driver.destroy_pipeline(cached.pipeline);
            self.driver.destroy_pipeline_layout(cached.pipeline_layout);
            self.driver.destroy_descriptor_set_layout(cached.set_layout);
            self.driver.destroy_shader_module(cached.module);
        }
        let buffers = std::mem::take(&mut self.buffers);
        for (_, buf) in buffers {
            self.driver.destroy_buffer(buf.buffer, buf.memory);
        }
        self.driver.destroy_command_pool(self.command_pool);
    }
}

impl Backend for VulkanBackend {
    fn name(&self) -> &str {
        "vulkan"
    }

    fn features(&self) -> FeatureSet {
        let features = FeatureSet::new()
            .with(Feature::ProgramFloat32)
            .with(Feature::Bindless)
            .with(Feature::DescriptorBuffer)
            .with(Feature::DynamicRendering)
            .with(Feature::Synchronization2);
        if self.supports_float64 { features.with(Feature::ProgramFloat64) } else { features }
    }

    fn prepare(&mut self, graph: &Graph) -> Result<()> {
        for node in &graph.resources.nodes {
            let bytes = node
                .byte_size
                .ok_or_else(|| map_backend_err(BackendError(format!("resource {} needs byte_size for vulkan backend", node.id.0))))?
                as usize;
            if let Some(existing) = self.buffers.get(&node.id) {
                if existing.bytes != bytes {
                    return Err(map_backend_err(BackendError(format!(
                        "resource {} size mismatch: have {}, need {bytes}",
                        node.id.0, existing.bytes
                    ))));
                }
                continue;
            }
            let (buffer, memory) = self.create_buffer(
                bytes,
                VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_TRANSFER_SRC_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT,
                VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT,
            )?;
            self.buffers.insert(node.id, DeviceBuffer { buffer, memory, bytes });
            let zeros = vec![0u8; bytes];
            self.upload(node.id, &zeros)?;
        }
        Ok(())
    }

    fn submit_task(&mut self, _graph: &Graph, task: &TaskNode) -> Result<()> {
        match &task.kind {
            TaskKind::Copy => self.run_copy(task),
            TaskKind::Fill => self.run_fill(task),
            TaskKind::MatMul => self.run_matmul(task),
            TaskKind::Dispatch => self.run_program_task(task),
            TaskKind::Raster => self.run_raster(task),
            TaskKind::RtTrace => Err(map_backend_err(BackendError("ray tracing is not in the vulkan thin gate".into()))),
            other => Err(map_backend_err(BackendError(format!("vulkan backend does not implement {other:?}")))),
        }
    }
}

fn expect_bytes(id: ResourceId, have: usize, need: usize) -> Result<()> {
    if have != need {
        return Err(map_backend_err(BackendError(format!("resource {} has {have} bytes, expected {need}", id.0))));
    }
    Ok(())
}

fn vk_err(error: DriverError) -> SchedulerError {
    SchedulerError::Backend("vulkan".into(), error.to_string())
}

fn map_emit_err(error: String) -> SchedulerError {
    SchedulerError::Backend("vulkan".into(), error)
}

fn map_backend_err(error: BackendError) -> SchedulerError {
    SchedulerError::Backend("vulkan".into(), error.0)
}
