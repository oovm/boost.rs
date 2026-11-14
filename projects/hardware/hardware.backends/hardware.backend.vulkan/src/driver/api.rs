#![allow(non_snake_case)]

//! Dynamically loaded Vulkan entry points owned by this backend.

use std::ffi::{c_void, CStr, CString};
use std::path::Path;

use libloading::Library;

use super::ffi::*;

macro_rules! load_fn {
    ($get:expr, $handle:expr, $name:literal, $ty:ty) => {{
        let name = CString::new($name).expect("vulkan symbol name");
        let raw = unsafe { ($get)($handle, name.as_ptr()) };
        if raw.is_null() {
            return Err(DriverError::new(-1, format!("missing symbol `{}`", $name)));
        }
        unsafe { std::mem::transmute::<*mut c_void, $ty>(raw) }
    }};
}

type CreateInstance = unsafe extern "C" fn(
    *const VkInstanceCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkInstance,
) -> VkResult;
type DestroyInstance = unsafe extern "C" fn(VkInstance, *const VkAllocationCallbacks);
type EnumeratePhysicalDevices =
    unsafe extern "C" fn(VkInstance, *mut u32, *mut VkPhysicalDevice) -> VkResult;
type GetPhysicalDeviceQueueFamilyProperties =
    unsafe extern "C" fn(VkPhysicalDevice, *mut u32, *mut VkQueueFamilyProperties);
type GetPhysicalDeviceFeatures =
    unsafe extern "C" fn(VkPhysicalDevice, *mut VkPhysicalDeviceFeatures);
type GetPhysicalDeviceMemoryProperties =
    unsafe extern "C" fn(VkPhysicalDevice, *mut VkPhysicalDeviceMemoryProperties);
type CreateDevice = unsafe extern "C" fn(
    VkPhysicalDevice,
    *const VkDeviceCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkDevice,
) -> VkResult;
type GetDeviceQueue = unsafe extern "C" fn(VkDevice, u32, u32, *mut VkQueue);

type DestroyDevice = unsafe extern "C" fn(VkDevice, *const VkAllocationCallbacks);
type DeviceWaitIdle = unsafe extern "C" fn(VkDevice) -> VkResult;
type CreateCommandPool = unsafe extern "C" fn(
    VkDevice,
    *const VkCommandPoolCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkCommandPool,
) -> VkResult;
type DestroyCommandPool =
    unsafe extern "C" fn(VkDevice, VkCommandPool, *const VkAllocationCallbacks);
type AllocateCommandBuffers = unsafe extern "C" fn(
    VkDevice,
    *const VkCommandBufferAllocateInfo,
    *mut VkCommandBuffer,
) -> VkResult;
type FreeCommandBuffers =
    unsafe extern "C" fn(VkDevice, VkCommandPool, u32, *const VkCommandBuffer);
type BeginCommandBuffer =
    unsafe extern "C" fn(VkCommandBuffer, *const VkCommandBufferBeginInfo) -> VkResult;
type EndCommandBuffer = unsafe extern "C" fn(VkCommandBuffer) -> VkResult;
type QueueSubmit =
    unsafe extern "C" fn(VkQueue, u32, *const VkSubmitInfo, u64) -> VkResult;
type QueueWaitIdle = unsafe extern "C" fn(VkQueue) -> VkResult;
type CreateBuffer = unsafe extern "C" fn(
    VkDevice,
    *const VkBufferCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkBuffer,
) -> VkResult;
type DestroyBuffer = unsafe extern "C" fn(VkDevice, VkBuffer, *const VkAllocationCallbacks);
type GetBufferMemoryRequirements =
    unsafe extern "C" fn(VkDevice, VkBuffer, *mut VkMemoryRequirements);
type AllocateMemory = unsafe extern "C" fn(
    VkDevice,
    *const VkMemoryAllocateInfo,
    *const VkAllocationCallbacks,
    *mut VkDeviceMemory,
) -> VkResult;
type FreeMemory = unsafe extern "C" fn(VkDevice, VkDeviceMemory, *const VkAllocationCallbacks);
type BindBufferMemory = unsafe extern "C" fn(VkDevice, VkBuffer, VkDeviceMemory, u64) -> VkResult;
type MapMemory =
    unsafe extern "C" fn(VkDevice, VkDeviceMemory, u64, u64, u32, *mut *mut c_void) -> VkResult;
type UnmapMemory = unsafe extern "C" fn(VkDevice, VkDeviceMemory);
type CreateImage = unsafe extern "C" fn(
    VkDevice,
    *const VkImageCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkImage,
) -> VkResult;
type DestroyImage = unsafe extern "C" fn(VkDevice, VkImage, *const VkAllocationCallbacks);
type GetImageMemoryRequirements =
    unsafe extern "C" fn(VkDevice, VkImage, *mut VkMemoryRequirements);
type BindImageMemory = unsafe extern "C" fn(VkDevice, VkImage, VkDeviceMemory, u64) -> VkResult;
type CreateImageView = unsafe extern "C" fn(
    VkDevice,
    *const VkImageViewCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkImageView,
) -> VkResult;
type DestroyImageView = unsafe extern "C" fn(VkDevice, VkImageView, *const VkAllocationCallbacks);
type CreateRenderPass = unsafe extern "C" fn(
    VkDevice,
    *const VkRenderPassCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkRenderPass,
) -> VkResult;
type DestroyRenderPass =
    unsafe extern "C" fn(VkDevice, VkRenderPass, *const VkAllocationCallbacks);
type CreateFramebuffer = unsafe extern "C" fn(
    VkDevice,
    *const VkFramebufferCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkFramebuffer,
) -> VkResult;
type DestroyFramebuffer =
    unsafe extern "C" fn(VkDevice, VkFramebuffer, *const VkAllocationCallbacks);
type CreateShaderModule = unsafe extern "C" fn(
    VkDevice,
    *const VkShaderModuleCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkShaderModule,
) -> VkResult;
type DestroyShaderModule =
    unsafe extern "C" fn(VkDevice, VkShaderModule, *const VkAllocationCallbacks);
type CreatePipelineLayout = unsafe extern "C" fn(
    VkDevice,
    *const VkPipelineLayoutCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkPipelineLayout,
) -> VkResult;
type DestroyPipelineLayout =
    unsafe extern "C" fn(VkDevice, VkPipelineLayout, *const VkAllocationCallbacks);
type CreateDescriptorSetLayout = unsafe extern "C" fn(
    VkDevice,
    *const VkDescriptorSetLayoutCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkDescriptorSetLayout,
) -> VkResult;
type DestroyDescriptorSetLayout =
    unsafe extern "C" fn(VkDevice, VkDescriptorSetLayout, *const VkAllocationCallbacks);
type CreateDescriptorPool = unsafe extern "C" fn(
    VkDevice,
    *const VkDescriptorPoolCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkDescriptorPool,
) -> VkResult;
type DestroyDescriptorPool =
    unsafe extern "C" fn(VkDevice, VkDescriptorPool, *const VkAllocationCallbacks);
type AllocateDescriptorSets = unsafe extern "C" fn(
    VkDevice,
    *const VkDescriptorSetAllocateInfo,
    *mut VkDescriptorSet,
) -> VkResult;
type UpdateDescriptorSets =
    unsafe extern "C" fn(VkDevice, u32, *const VkWriteDescriptorSet, u32, *const c_void);
type CreateComputePipelines = unsafe extern "C" fn(
    VkDevice,
    VkPipeline,
    u32,
    *const VkComputePipelineCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkPipeline,
) -> VkResult;
type CreateGraphicsPipelines = unsafe extern "C" fn(
    VkDevice,
    VkPipeline,
    u32,
    *const VkGraphicsPipelineCreateInfo,
    *const VkAllocationCallbacks,
    *mut VkPipeline,
) -> VkResult;
type DestroyPipeline = unsafe extern "C" fn(VkDevice, VkPipeline, *const VkAllocationCallbacks);
type CmdPipelineBarrier = unsafe extern "C" fn(
    VkCommandBuffer,
    u32,
    u32,
    u32,
    u32,
    *const VkMemoryBarrier,
    u32,
    *const c_void,
    u32,
    *const VkImageMemoryBarrier,
);
type CmdClearColorImage = unsafe extern "C" fn(
    VkCommandBuffer,
    VkImage,
    u32,
    *const VkClearColorValue,
    u32,
    *const VkImageSubresourceRange,
);
type CmdCopyBuffer =
    unsafe extern "C" fn(VkCommandBuffer, VkBuffer, VkBuffer, u32, *const VkBufferCopy);
type CmdCopyImageToBuffer =
    unsafe extern "C" fn(VkCommandBuffer, VkImage, u32, VkBuffer, u32, *const VkBufferImageCopy);
type CmdBeginRenderPass =
    unsafe extern "C" fn(VkCommandBuffer, *const VkRenderPassBeginInfo, u32);
type CmdEndRenderPass = unsafe extern "C" fn(VkCommandBuffer);
type CmdBindPipeline = unsafe extern "C" fn(VkCommandBuffer, u32, VkPipeline);
type CmdBindDescriptorSets = unsafe extern "C" fn(
    VkCommandBuffer,
    u32,
    VkPipelineLayout,
    u32,
    u32,
    *const VkDescriptorSet,
    u32,
    *const u32,
);
type CmdPushConstants =
    unsafe extern "C" fn(VkCommandBuffer, VkPipelineLayout, u32, u32, u32, *const c_void);
type CmdDispatch = unsafe extern "C" fn(VkCommandBuffer, u32, u32, u32);
type CmdDraw = unsafe extern "C" fn(VkCommandBuffer, u32, u32, u32, u32);

struct InstanceFns {
    destroy_instance: DestroyInstance,
    enumerate_physical_devices: EnumeratePhysicalDevices,
    get_physical_device_queue_family_properties: GetPhysicalDeviceQueueFamilyProperties,
    get_physical_device_features: GetPhysicalDeviceFeatures,
    get_physical_device_memory_properties: GetPhysicalDeviceMemoryProperties,
    create_device: CreateDevice,
}

struct DeviceFns {
    get_device_queue: GetDeviceQueue,
    destroy_device: DestroyDevice,
    device_wait_idle: DeviceWaitIdle,
    create_command_pool: CreateCommandPool,
    destroy_command_pool: DestroyCommandPool,
    allocate_command_buffers: AllocateCommandBuffers,
    free_command_buffers: FreeCommandBuffers,
    begin_command_buffer: BeginCommandBuffer,
    end_command_buffer: EndCommandBuffer,
    queue_submit: QueueSubmit,
    queue_wait_idle: QueueWaitIdle,
    create_buffer: CreateBuffer,
    destroy_buffer: DestroyBuffer,
    get_buffer_memory_requirements: GetBufferMemoryRequirements,
    allocate_memory: AllocateMemory,
    free_memory: FreeMemory,
    bind_buffer_memory: BindBufferMemory,
    map_memory: MapMemory,
    unmap_memory: UnmapMemory,
    create_image: CreateImage,
    destroy_image: DestroyImage,
    get_image_memory_requirements: GetImageMemoryRequirements,
    bind_image_memory: BindImageMemory,
    create_image_view: CreateImageView,
    destroy_image_view: DestroyImageView,
    create_render_pass: CreateRenderPass,
    destroy_render_pass: DestroyRenderPass,
    create_framebuffer: CreateFramebuffer,
    destroy_framebuffer: DestroyFramebuffer,
    create_shader_module: CreateShaderModule,
    destroy_shader_module: DestroyShaderModule,
    create_pipeline_layout: CreatePipelineLayout,
    destroy_pipeline_layout: DestroyPipelineLayout,
    create_descriptor_set_layout: CreateDescriptorSetLayout,
    destroy_descriptor_set_layout: DestroyDescriptorSetLayout,
    create_descriptor_pool: CreateDescriptorPool,
    destroy_descriptor_pool: DestroyDescriptorPool,
    allocate_descriptor_sets: AllocateDescriptorSets,
    update_descriptor_sets: UpdateDescriptorSets,
    create_compute_pipelines: CreateComputePipelines,
    create_graphics_pipelines: CreateGraphicsPipelines,
    destroy_pipeline: DestroyPipeline,
    cmd_pipeline_barrier: CmdPipelineBarrier,
    cmd_clear_color_image: CmdClearColorImage,
    cmd_copy_buffer: CmdCopyBuffer,
    cmd_copy_image_to_buffer: CmdCopyImageToBuffer,
    cmd_begin_render_pass: CmdBeginRenderPass,
    cmd_end_render_pass: CmdEndRenderPass,
    cmd_bind_pipeline: CmdBindPipeline,
    cmd_bind_descriptor_sets: CmdBindDescriptorSets,
    cmd_push_constants: CmdPushConstants,
    cmd_dispatch: CmdDispatch,
    cmd_draw: CmdDraw,
}

/// Loaded Vulkan instance/device pair plus the entry points the backend needs.
pub struct VulkanDriver {
    _lib: Library,
    instance_fns: InstanceFns,
    device_fns: DeviceFns,
    instance: VkInstance,
    device: VkDevice,
    queue: VkQueue,
    queue_family: u32,
    graphics: bool,
    supports_float64: bool,
    memory: VkPhysicalDeviceMemoryProperties,
}

impl VulkanDriver {
    /// Open the first physical device with a graphics or compute queue.
    pub fn new() -> Result<Self, DriverError> {
        let lib = Self::load_library()?;
        unsafe {
            let get_instance_proc_addr: PFN_vkGetInstanceProcAddr = {
                let sym: libloading::Symbol<PFN_vkGetInstanceProcAddr> =
                    lib.get(b"vkGetInstanceProcAddr").map_err(|error| {
                        DriverError::new(-1, format!("missing `vkGetInstanceProcAddr`: {error}"))
                    })?;
                *sym
            };
            let create_instance: CreateInstance =
                load_fn!(get_instance_proc_addr, std::ptr::null_mut(), "vkCreateInstance", CreateInstance);

            let app_name = CString::new("hardware").expect("literal");
            let engine_name = CString::new("hardware").expect("literal");
            let app = VkApplicationInfo {
                p_application_name: app_name.as_ptr(),
                p_engine_name: engine_name.as_ptr(),
                api_version: (1 << 22) | (1 << 12),
                ..VkApplicationInfo::default()
            };
            let info = VkInstanceCreateInfo {
                p_application_info: &app,
                ..VkInstanceCreateInfo::default()
            };
            let mut instance: VkInstance = std::ptr::null_mut();
            check(
                create_instance(&info, std::ptr::null(), &mut instance),
                "vkCreateInstance",
            )?;

            let instance_fns = InstanceFns {
                destroy_instance: load_fn!(get_instance_proc_addr, instance, "vkDestroyInstance", DestroyInstance),
                enumerate_physical_devices: load_fn!(get_instance_proc_addr, instance, "vkEnumeratePhysicalDevices", EnumeratePhysicalDevices),
                get_physical_device_queue_family_properties: load_fn!(get_instance_proc_addr, instance, "vkGetPhysicalDeviceQueueFamilyProperties", GetPhysicalDeviceQueueFamilyProperties),
                get_physical_device_features: load_fn!(get_instance_proc_addr, instance, "vkGetPhysicalDeviceFeatures", GetPhysicalDeviceFeatures),
                get_physical_device_memory_properties: load_fn!(get_instance_proc_addr, instance, "vkGetPhysicalDeviceMemoryProperties", GetPhysicalDeviceMemoryProperties),
                create_device: load_fn!(get_instance_proc_addr, instance, "vkCreateDevice", CreateDevice),
            };

            let physicals = enumerate_physical_devices(instance, &instance_fns)?;
            let (physical, queue_family, graphics) =
                pick_queue_family(instance, &instance_fns, &physicals)?;

            let mut features = VkPhysicalDeviceFeatures::default();
            (instance_fns.get_physical_device_features)(physical, &mut features);
            let supports_float64 = features.bits[FEATURE_SHADER_FLOAT64] != 0;
            let mut enabled = VkPhysicalDeviceFeatures::default();
            enabled.bits[FEATURE_SHADER_FLOAT64] = u32::from(supports_float64);

            let priorities = [1.0f32];
            let queue_info = VkDeviceQueueCreateInfo {
                queue_family_index: queue_family,
                queue_count: 1,
                p_queue_priorities: priorities.as_ptr(),
                ..VkDeviceQueueCreateInfo::default()
            };
            let device_info = VkDeviceCreateInfo {
                queue_create_info_count: 1,
                p_queue_create_infos: &queue_info,
                p_enabled_features: &enabled,
                ..VkDeviceCreateInfo::default()
            };
            let mut device: VkDevice = std::ptr::null_mut();
            check(
                (instance_fns.create_device)(physical, &device_info, std::ptr::null(), &mut device),
                "vkCreateDevice",
            )?;

            let get_device_proc_addr: PFN_vkGetDeviceProcAddr = load_fn!(
                get_instance_proc_addr,
                instance,
                "vkGetDeviceProcAddr",
                PFN_vkGetDeviceProcAddr
            );
            let device_fns = DeviceFns::load(get_device_proc_addr, device)?;

            let mut queue: VkQueue = std::ptr::null_mut();
            (device_fns.get_device_queue)(device, queue_family, 0, &mut queue);

            let mut memory = VkPhysicalDeviceMemoryProperties::default();
            (instance_fns.get_physical_device_memory_properties)(physical, &mut memory);

            Ok(Self {
                _lib: lib,
                instance_fns,
                device_fns,
                instance,
                device,
                queue,
                queue_family,
                graphics,
                supports_float64,
                memory,
            })
        }
    }

    fn load_library() -> Result<Library, DriverError> {
        // `vulkan-1.dll` / `libvulkan.so.1` are installed by the GPU driver, not
        // by the operating system. Apple platforms ship no Vulkan loader at all.
        let candidates: &[&str] = if cfg!(target_os = "windows") {
            &["vulkan-1.dll"]
        } else if cfg!(target_os = "macos") {
            &[]
        } else {
            &["libvulkan.so.1", "libvulkan.so"]
        };
        let mut last = String::from("no candidate");
        for candidate in candidates {
            match unsafe { Library::new(Path::new(candidate)) } {
                Ok(lib) => return Ok(lib),
                Err(error) => last = format!("{candidate}: {error}"),
            }
        }
        Err(DriverError::new(-1, format!("failed to load Vulkan loader: {last}")))
    }

    /// Whether the selected queue family supports graphics.
    pub fn graphics(&self) -> bool {
        self.graphics
    }

    /// Whether the physical device exposes `shaderFloat64`.
    pub fn supports_float64(&self) -> bool {
        self.supports_float64
    }

    /// Pick a memory type satisfying `bits` and `props`.
    pub fn memory_type(&self, bits: u32, props: u32) -> Option<u32> {
        (0..self.memory.memory_type_count).find(|index| {
            let ty = self.memory.memory_types[*index as usize];
            (bits & (1 << *index)) != 0 && (ty.property_flags & props) == props
        })
    }

    /// Create a command pool.
    pub fn create_command_pool(&self, flags: u32) -> Result<VkCommandPool, DriverError> {
        let info = VkCommandPoolCreateInfo {
            flags,
            queue_family_index: self.queue_family,
            ..VkCommandPoolCreateInfo::default()
        };
        let mut pool: VkCommandPool = 0;
        unsafe {
            check(
                (self.device_fns.create_command_pool)(
                    self.device,
                    &info,
                    std::ptr::null(),
                    &mut pool,
                ),
                "vkCreateCommandPool",
            )?;
        }
        Ok(pool)
    }

    /// Destroy a command pool.
    pub fn destroy_command_pool(&self, pool: VkCommandPool) {
        unsafe { (self.device_fns.destroy_command_pool)(self.device, pool, std::ptr::null()) }
    }

    /// Allocate one primary command buffer.
    pub fn allocate_command_buffer(&self, pool: VkCommandPool) -> Result<VkCommandBuffer, DriverError> {
        let info = VkCommandBufferAllocateInfo {
            command_pool: pool,
            level: VK_COMMAND_BUFFER_LEVEL_PRIMARY,
            command_buffer_count: 1,
            ..VkCommandBufferAllocateInfo::default()
        };
        let mut buffer: VkCommandBuffer = std::ptr::null_mut();
        unsafe {
            check(
                (self.device_fns.allocate_command_buffers)(self.device, &info, &mut buffer),
                "vkAllocateCommandBuffers",
            )?;
        }
        Ok(buffer)
    }

    /// Free an allocated command buffer.
    pub fn free_command_buffer(&self, pool: VkCommandPool, buffer: VkCommandBuffer) {
        unsafe { (self.device_fns.free_command_buffers)(self.device, pool, 1, &buffer) }
    }

    /// Begin recording a command buffer.
    pub fn begin_command_buffer(&self, buffer: VkCommandBuffer) -> Result<(), DriverError> {
        let info = VkCommandBufferBeginInfo {
            flags: VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
            ..VkCommandBufferBeginInfo::default()
        };
        unsafe { check((self.device_fns.begin_command_buffer)(buffer, &info), "vkBeginCommandBuffer").map(|_| ()) }
    }

    /// End recording a command buffer.
    pub fn end_command_buffer(&self, buffer: VkCommandBuffer) -> Result<(), DriverError> {
        unsafe { check((self.device_fns.end_command_buffer)(buffer), "vkEndCommandBuffer").map(|_| ()) }
    }

    /// Submit one command buffer to the owned queue and wait for completion.
    pub fn submit_and_wait(&self, buffer: VkCommandBuffer) -> Result<(), DriverError> {
        let submit = VkSubmitInfo {
            command_buffer_count: 1,
            p_command_buffers: &buffer,
            ..VkSubmitInfo::default()
        };
        unsafe {
            check(
                (self.device_fns.queue_submit)(self.queue, 1, &submit, 0),
                "vkQueueSubmit",
            )?;
            check((self.device_fns.queue_wait_idle)(self.queue), "vkQueueWaitIdle")?;
        }
        Ok(())
    }

    /// Wait for all device work to finish.
    pub fn device_wait_idle(&self) -> Result<(), DriverError> {
        unsafe { check((self.device_fns.device_wait_idle)(self.device), "vkDeviceWaitIdle").map(|_| ()) }
    }

    /// Create a buffer and bind memory matching `props`.
    pub fn create_buffer(
        &self,
        bytes: usize,
        usage: u32,
        props: u32,
    ) -> Result<(VkBuffer, VkDeviceMemory), DriverError> {
        unsafe {
            let info = VkBufferCreateInfo {
                size: bytes.max(1) as u64,
                usage,
                ..VkBufferCreateInfo::default()
            };
            let mut buffer: VkBuffer = 0;
            check(
                (self.device_fns.create_buffer)(self.device, &info, std::ptr::null(), &mut buffer),
                "vkCreateBuffer",
            )?;
            let mut requirements = VkMemoryRequirements::default();
            (self.device_fns.get_buffer_memory_requirements)(self.device, buffer, &mut requirements);
            let memory_type = self.memory_type(requirements.memory_type_bits, props).ok_or_else(|| {
                DriverError::new(-1, "no suitable buffer memory type")
            })?;
            let allocation = VkMemoryAllocateInfo {
                allocation_size: requirements.size,
                memory_type_index: memory_type,
                ..VkMemoryAllocateInfo::default()
            };
            let mut memory: VkDeviceMemory = 0;
            if let Err(error) = check(
                (self.device_fns.allocate_memory)(
                    self.device,
                    &allocation,
                    std::ptr::null(),
                    &mut memory,
                ),
                "vkAllocateMemory",
            ) {
                (self.device_fns.destroy_buffer)(self.device, buffer, std::ptr::null());
                return Err(error);
            }
            if let Err(error) =
                check((self.device_fns.bind_buffer_memory)(self.device, buffer, memory, 0), "vkBindBufferMemory")
            {
                (self.device_fns.free_memory)(self.device, memory, std::ptr::null());
                (self.device_fns.destroy_buffer)(self.device, buffer, std::ptr::null());
                return Err(error);
            }
            Ok((buffer, memory))
        }
    }

    /// Destroy a buffer and free its memory.
    pub fn destroy_buffer(&self, buffer: VkBuffer, memory: VkDeviceMemory) {
        unsafe {
            (self.device_fns.destroy_buffer)(self.device, buffer, std::ptr::null());
            (self.device_fns.free_memory)(self.device, memory, std::ptr::null());
        }
    }

    /// Create a `R8G8B8A8` image and bind device-local memory.
    pub fn create_image(
        &self,
        width: u32,
        height: u32,
        usage: u32,
        props: u32,
    ) -> Result<(VkImage, VkDeviceMemory), DriverError> {
        unsafe {
            let info = VkImageCreateInfo {
                extent: VkExtent3D {
                    width,
                    height,
                    depth: 1,
                },
                usage,
                ..VkImageCreateInfo::default()
            };
            let mut image: VkImage = 0;
            check(
                (self.device_fns.create_image)(self.device, &info, std::ptr::null(), &mut image),
                "vkCreateImage",
            )?;
            let mut requirements = VkMemoryRequirements::default();
            (self.device_fns.get_image_memory_requirements)(self.device, image, &mut requirements);
            let memory_type = self.memory_type(requirements.memory_type_bits, props).ok_or_else(|| {
                DriverError::new(-1, "no suitable image memory type")
            })?;
            let allocation = VkMemoryAllocateInfo {
                allocation_size: requirements.size,
                memory_type_index: memory_type,
                ..VkMemoryAllocateInfo::default()
            };
            let mut memory: VkDeviceMemory = 0;
            if let Err(error) = check(
                (self.device_fns.allocate_memory)(
                    self.device,
                    &allocation,
                    std::ptr::null(),
                    &mut memory,
                ),
                "vkAllocateMemory",
            ) {
                (self.device_fns.destroy_image)(self.device, image, std::ptr::null());
                return Err(error);
            }
            if let Err(error) =
                check((self.device_fns.bind_image_memory)(self.device, image, memory, 0), "vkBindImageMemory")
            {
                (self.device_fns.free_memory)(self.device, memory, std::ptr::null());
                (self.device_fns.destroy_image)(self.device, image, std::ptr::null());
                return Err(error);
            }
            Ok((image, memory))
        }
    }

    /// Destroy an image and free its memory.
    pub fn destroy_image(&self, image: VkImage, memory: VkDeviceMemory) {
        unsafe {
            (self.device_fns.destroy_image)(self.device, image, std::ptr::null());
            (self.device_fns.free_memory)(self.device, memory, std::ptr::null());
        }
    }

    /// Create a 2D image view over the whole image.
    pub fn create_image_view(&self, image: VkImage) -> Result<VkImageView, DriverError> {
        let info = VkImageViewCreateInfo {
            image,
            view_type: VK_IMAGE_VIEW_TYPE_2D,
            format: VK_FORMAT_R8G8B8A8_UNORM,
            subresource_range: VkImageSubresourceRange {
                aspect_mask: VK_IMAGE_ASPECT_COLOR_BIT,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            },
            ..VkImageViewCreateInfo::default()
        };
        let mut view: VkImageView = 0;
        unsafe {
            check(
                (self.device_fns.create_image_view)(self.device, &info, std::ptr::null(), &mut view),
                "vkCreateImageView",
            )?;
        }
        Ok(view)
    }

    /// Destroy an image view.
    pub fn destroy_image_view(&self, view: VkImageView) {
        unsafe { (self.device_fns.destroy_image_view)(self.device, view, std::ptr::null()) }
    }

    /// Create a colour-only render pass that ends in `final_layout`.
    pub fn create_color_render_pass(
        &self,
        final_layout: u32,
    ) -> Result<VkRenderPass, DriverError> {
        let attachment = VkAttachmentDescription {
            format: VK_FORMAT_R8G8B8A8_UNORM,
            samples: VK_SAMPLE_COUNT_1_BIT,
            load_op: VK_ATTACHMENT_LOAD_OP_CLEAR,
            store_op: VK_ATTACHMENT_STORE_OP_STORE,
            stencil_load_op: VK_ATTACHMENT_LOAD_OP_DONT_CARE,
            stencil_store_op: VK_ATTACHMENT_STORE_OP_DONT_CARE,
            initial_layout: VK_IMAGE_LAYOUT_UNDEFINED,
            final_layout,
            ..VkAttachmentDescription::default()
        };
        let color = VkAttachmentReference {
            attachment: 0,
            layout: VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
        };
        let subpass = VkSubpassDescription {
            color_attachment_count: 1,
            p_color_attachments: &color,
            ..VkSubpassDescription::default()
        };
        let dependency = VkSubpassDependency {
            src_subpass: VK_SUBPASS_EXTERNAL,
            dst_subpass: 0,
            src_stage_mask: VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
            dst_stage_mask: VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
            dst_access_mask: VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
            ..VkSubpassDependency::default()
        };
        let info = VkRenderPassCreateInfo {
            attachment_count: 1,
            p_attachments: &attachment,
            subpass_count: 1,
            p_subpasses: &subpass,
            dependency_count: 1,
            p_dependencies: &dependency,
            ..VkRenderPassCreateInfo::default()
        };
        let mut pass: VkRenderPass = 0;
        unsafe {
            check(
                (self.device_fns.create_render_pass)(self.device, &info, std::ptr::null(), &mut pass),
                "vkCreateRenderPass",
            )?;
        }
        Ok(pass)
    }

    /// Destroy a render pass.
    pub fn destroy_render_pass(&self, pass: VkRenderPass) {
        unsafe { (self.device_fns.destroy_render_pass)(self.device, pass, std::ptr::null()) }
    }

    /// Create a framebuffer with one colour attachment.
    pub fn create_framebuffer(
        &self,
        pass: VkRenderPass,
        view: VkImageView,
        width: u32,
        height: u32,
    ) -> Result<VkFramebuffer, DriverError> {
        let info = VkFramebufferCreateInfo {
            render_pass: pass,
            attachment_count: 1,
            p_attachments: &view,
            width,
            height,
            layers: 1,
            ..VkFramebufferCreateInfo::default()
        };
        let mut framebuffer: VkFramebuffer = 0;
        unsafe {
            check(
                (self.device_fns.create_framebuffer)(self.device, &info, std::ptr::null(), &mut framebuffer),
                "vkCreateFramebuffer",
            )?;
        }
        Ok(framebuffer)
    }

    /// Destroy a framebuffer.
    pub fn destroy_framebuffer(&self, framebuffer: VkFramebuffer) {
        unsafe { (self.device_fns.destroy_framebuffer)(self.device, framebuffer, std::ptr::null()) }
    }

    /// Create a shader module from SPIR-V words.
    pub fn create_shader_module(&self, words: &[u32]) -> Result<VkShaderModule, DriverError> {
        let info = VkShaderModuleCreateInfo {
            code_size: words.len() * 4,
            p_code: words.as_ptr(),
            ..VkShaderModuleCreateInfo::default()
        };
        let mut module: VkShaderModule = 0;
        unsafe {
            check(
                (self.device_fns.create_shader_module)(self.device, &info, std::ptr::null(), &mut module),
                "vkCreateShaderModule",
            )?;
        }
        Ok(module)
    }

    /// Destroy a shader module.
    pub fn destroy_shader_module(&self, module: VkShaderModule) {
        unsafe { (self.device_fns.destroy_shader_module)(self.device, module, std::ptr::null()) }
    }

    /// Create a descriptor set layout with consecutive storage buffers.
    pub fn create_storage_set_layout(
        &self,
        bindings: u32,
        stage: u32,
    ) -> Result<VkDescriptorSetLayout, DriverError> {
        let entries: Vec<VkDescriptorSetLayoutBinding> = (0..bindings)
            .map(|binding| VkDescriptorSetLayoutBinding {
                binding,
                descriptor_type: VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
                descriptor_count: 1,
                stage_flags: stage,
                p_immutable_samplers: std::ptr::null(),
            })
            .collect();
        let info = VkDescriptorSetLayoutCreateInfo {
            binding_count: entries.len() as u32,
            p_bindings: entries.as_ptr(),
            ..VkDescriptorSetLayoutCreateInfo::default()
        };
        let mut layout: VkDescriptorSetLayout = 0;
        unsafe {
            check(
                (self.device_fns.create_descriptor_set_layout)(self.device, &info, std::ptr::null(), &mut layout),
                "vkCreateDescriptorSetLayout",
            )?;
        }
        Ok(layout)
    }

    /// Destroy a descriptor set layout.
    pub fn destroy_descriptor_set_layout(&self, layout: VkDescriptorSetLayout) {
        unsafe { (self.device_fns.destroy_descriptor_set_layout)(self.device, layout, std::ptr::null()) }
    }

    /// Create a pipeline layout.
    pub fn create_pipeline_layout(
        &self,
        set_layout: Option<VkDescriptorSetLayout>,
        push_size: u32,
        stage: u32,
    ) -> Result<VkPipelineLayout, DriverError> {
        let sets = set_layout.map(|layout| [layout]);
        let range = [VkPushConstantRange {
            stage_flags: stage,
            offset: 0,
            size: push_size,
        }];
        let info = VkPipelineLayoutCreateInfo {
            set_layout_count: sets.is_some() as u32,
            p_set_layouts: sets.as_ref().map_or(std::ptr::null(), |sets| sets.as_ptr()),
            push_constant_range_count: u32::from(push_size > 0),
            p_push_constant_ranges: if push_size > 0 { range.as_ptr() } else { std::ptr::null() },
            ..VkPipelineLayoutCreateInfo::default()
        };
        let mut layout: VkPipelineLayout = 0;
        unsafe {
            check(
                (self.device_fns.create_pipeline_layout)(self.device, &info, std::ptr::null(), &mut layout),
                "vkCreatePipelineLayout",
            )?;
        }
        Ok(layout)
    }

    /// Destroy a pipeline layout.
    pub fn destroy_pipeline_layout(&self, layout: VkPipelineLayout) {
        unsafe { (self.device_fns.destroy_pipeline_layout)(self.device, layout, std::ptr::null()) }
    }

    /// Create a compute pipeline for `entry`.
    pub fn create_compute_pipeline(
        &self,
        module: VkShaderModule,
        entry: &CStr,
        layout: VkPipelineLayout,
    ) -> Result<VkPipeline, DriverError> {
        let stage = VkPipelineShaderStageCreateInfo {
            stage: VK_SHADER_STAGE_COMPUTE_BIT,
            module,
            p_name: entry.as_ptr(),
            ..VkPipelineShaderStageCreateInfo::default()
        };
        let info = VkComputePipelineCreateInfo {
            stage,
            layout,
            ..VkComputePipelineCreateInfo::default()
        };
        let mut pipeline: VkPipeline = 0;
        unsafe {
            check(
                (self.device_fns.create_compute_pipelines)(
                    self.device,
                    0,
                    1,
                    &info,
                    std::ptr::null(),
                    &mut pipeline,
                ),
                "vkCreateComputePipelines",
            )?;
        }
        Ok(pipeline)
    }

    /// Create a graphics pipeline that draws a full triangle list.
    pub fn create_triangle_pipeline(
        &self,
        vertex: VkShaderModule,
        vertex_entry: &CStr,
        fragment: VkShaderModule,
        fragment_entry: &CStr,
        layout: VkPipelineLayout,
        pass: VkRenderPass,
        width: u32,
        height: u32,
    ) -> Result<VkPipeline, DriverError> {
        let stages = [
            VkPipelineShaderStageCreateInfo {
                stage: VK_SHADER_STAGE_VERTEX_BIT,
                module: vertex,
                p_name: vertex_entry.as_ptr(),
                ..VkPipelineShaderStageCreateInfo::default()
            },
            VkPipelineShaderStageCreateInfo {
                stage: VK_SHADER_STAGE_FRAGMENT_BIT,
                module: fragment,
                p_name: fragment_entry.as_ptr(),
                ..VkPipelineShaderStageCreateInfo::default()
            },
        ];
        let vertex_input = VkPipelineVertexInputStateCreateInfo::default();
        let input_assembly = VkPipelineInputAssemblyStateCreateInfo::default();
        let viewport = VkViewport {
            x: 0.0,
            y: 0.0,
            width: width as f32,
            height: height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        };
        let scissor = VkRect2D {
            offset: VkOffset2D { x: 0, y: 0 },
            extent: VkExtent2D { width, height },
        };
        let viewport_state = VkPipelineViewportStateCreateInfo {
            viewport_count: 1,
            p_viewports: &viewport,
            scissor_count: 1,
            p_scissors: &scissor,
            ..VkPipelineViewportStateCreateInfo::default()
        };
        let raster = VkPipelineRasterizationStateCreateInfo::default();
        let multisample = VkPipelineMultisampleStateCreateInfo::default();
        let blend_attachment = VkPipelineColorBlendAttachmentState {
            blend_enable: 0,
            color_write_mask: VK_COLOR_COMPONENT_RGBA,
            ..VkPipelineColorBlendAttachmentState::default()
        };
        let color_blend = VkPipelineColorBlendStateCreateInfo {
            attachment_count: 1,
            p_attachments: &blend_attachment,
            ..VkPipelineColorBlendStateCreateInfo::default()
        };
        let info = VkGraphicsPipelineCreateInfo {
            stage_count: stages.len() as u32,
            p_stages: stages.as_ptr(),
            p_vertex_input_state: &vertex_input,
            p_input_assembly_state: &input_assembly,
            p_viewport_state: &viewport_state,
            p_rasterization_state: &raster,
            p_multisample_state: &multisample,
            p_color_blend_state: &color_blend,
            layout,
            render_pass: pass,
            subpass: 0,
            ..VkGraphicsPipelineCreateInfo::default()
        };
        let mut pipeline: VkPipeline = 0;
        unsafe {
            check(
                (self.device_fns.create_graphics_pipelines)(
                    self.device,
                    0,
                    1,
                    &info,
                    std::ptr::null(),
                    &mut pipeline,
                ),
                "vkCreateGraphicsPipelines",
            )?;
        }
        Ok(pipeline)
    }

    /// Destroy a pipeline.
    pub fn destroy_pipeline(&self, pipeline: VkPipeline) {
        unsafe { (self.device_fns.destroy_pipeline)(self.device, pipeline, std::ptr::null()) }
    }

    /// Create a descriptor pool sized for storage buffers.
    pub fn create_descriptor_pool(&self, buffers: u32) -> Result<VkDescriptorPool, DriverError> {
        let sizes = [VkDescriptorPoolSize {
            ty: VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
            descriptor_count: buffers,
        }];
        let info = VkDescriptorPoolCreateInfo {
            max_sets: 1,
            pool_size_count: 1,
            p_pool_sizes: sizes.as_ptr(),
            ..VkDescriptorPoolCreateInfo::default()
        };
        let mut pool: VkDescriptorPool = 0;
        unsafe {
            check(
                (self.device_fns.create_descriptor_pool)(self.device, &info, std::ptr::null(), &mut pool),
                "vkCreateDescriptorPool",
            )?;
        }
        Ok(pool)
    }

    /// Destroy a descriptor pool.
    pub fn destroy_descriptor_pool(&self, pool: VkDescriptorPool) {
        unsafe { (self.device_fns.destroy_descriptor_pool)(self.device, pool, std::ptr::null()) }
    }

    /// Allocate one descriptor set from `pool`.
    pub fn allocate_descriptor_set(
        &self,
        pool: VkDescriptorPool,
        layout: VkDescriptorSetLayout,
    ) -> Result<VkDescriptorSet, DriverError> {
        let info = VkDescriptorSetAllocateInfo {
            descriptor_pool: pool,
            descriptor_set_count: 1,
            p_set_layouts: &layout,
            ..VkDescriptorSetAllocateInfo::default()
        };
        let mut set: VkDescriptorSet = 0;
        unsafe {
            check(
                (self.device_fns.allocate_descriptor_sets)(self.device, &info, &mut set),
                "vkAllocateDescriptorSets",
            )?;
        }
        Ok(set)
    }

    /// Bind consecutive storage buffers into one descriptor set.
    pub fn update_storage_buffers(&self, set: VkDescriptorSet, buffers: &[VkBuffer]) {
        let infos: Vec<VkDescriptorBufferInfo> = buffers
            .iter()
            .map(|buffer| VkDescriptorBufferInfo {
                buffer: *buffer,
                offset: 0,
                range: VK_WHOLE_SIZE,
            })
            .collect();
        let writes: Vec<VkWriteDescriptorSet> = infos
            .iter()
            .enumerate()
            .map(|(index, info)| VkWriteDescriptorSet {
                dst_set: set,
                dst_binding: index as u32,
                descriptor_count: 1,
                descriptor_type: VK_DESCRIPTOR_TYPE_STORAGE_BUFFER,
                p_buffer_info: info,
                ..VkWriteDescriptorSet::default()
            })
            .collect();
        unsafe {
            (self.device_fns.update_descriptor_sets)(
                self.device,
                writes.len() as u32,
                writes.as_ptr(),
                0,
                std::ptr::null(),
            );
        }
    }

    /// Map `memory` for host access.
    pub fn map_memory(&self, memory: VkDeviceMemory, bytes: usize) -> Result<*mut c_void, DriverError> {
        let mut pointer: *mut c_void = std::ptr::null_mut();
        unsafe {
            check(
                (self.device_fns.map_memory)(
                    self.device,
                    memory,
                    0,
                    bytes.max(1) as u64,
                    0,
                    &mut pointer,
                ),
                "vkMapMemory",
            )?;
        }
        Ok(pointer)
    }

    /// Unmap host-visible memory.
    pub fn unmap_memory(&self, memory: VkDeviceMemory) {
        unsafe { (self.device_fns.unmap_memory)(self.device, memory) }
    }

    /// Record a memory barrier.
    pub fn cmd_memory_barrier(
        &self,
        buffer: VkCommandBuffer,
        src_stage: u32,
        dst_stage: u32,
        src_access: u32,
        dst_access: u32,
    ) {
        let barrier = VkMemoryBarrier {
            src_access_mask: src_access,
            dst_access_mask: dst_access,
            ..VkMemoryBarrier::default()
        };
        unsafe {
            (self.device_fns.cmd_pipeline_barrier)(
                buffer,
                src_stage,
                dst_stage,
                0,
                1,
                &barrier,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
            );
        }
    }

    /// Record an image memory barrier over the whole image.
    pub fn cmd_image_barrier(
        &self,
        buffer: VkCommandBuffer,
        image: VkImage,
        src_stage: u32,
        dst_stage: u32,
        src_access: u32,
        dst_access: u32,
        old_layout: u32,
        new_layout: u32,
    ) {
        let barrier = VkImageMemoryBarrier {
            src_access_mask: src_access,
            dst_access_mask: dst_access,
            old_layout,
            new_layout,
            image,
            subresource_range: VkImageSubresourceRange {
                aspect_mask: VK_IMAGE_ASPECT_COLOR_BIT,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            },
            ..VkImageMemoryBarrier::default()
        };
        unsafe {
            (self.device_fns.cmd_pipeline_barrier)(
                buffer,
                src_stage,
                dst_stage,
                0,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                1,
                &barrier,
            );
        }
    }

    /// Record a colour image clear.
    pub fn cmd_clear_color(&self, buffer: VkCommandBuffer, image: VkImage, color: [f32; 4]) {
        let value = VkClearColorValue { float32: color };
        let range = VkImageSubresourceRange {
            aspect_mask: VK_IMAGE_ASPECT_COLOR_BIT,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        unsafe {
            (self.device_fns.cmd_clear_color_image)(
                buffer,
                image,
                VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                &value,
                1,
                &range,
            );
        }
    }

    /// Record a buffer-to-buffer copy.
    pub fn cmd_copy_buffer(&self, buffer: VkCommandBuffer, src: VkBuffer, dst: VkBuffer, bytes: usize) {
        let region = VkBufferCopy {
            src_offset: 0,
            dst_offset: 0,
            size: bytes as u64,
        };
        unsafe { (self.device_fns.cmd_copy_buffer)(buffer, src, dst, 1, &region) }
    }

    /// Record an image-to-buffer copy.
    pub fn cmd_copy_image_to_buffer(
        &self,
        buffer: VkCommandBuffer,
        image: VkImage,
        dst: VkBuffer,
        width: u32,
        height: u32,
    ) {
        let region = VkBufferImageCopy {
            buffer_offset: 0,
            buffer_row_length: 0,
            buffer_image_height: 0,
            image_subresource: VkImageSubresourceLayers {
                aspect_mask: VK_IMAGE_ASPECT_COLOR_BIT,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            },
            image_offset: VkOffset3D { x: 0, y: 0, z: 0 },
            image_extent: VkExtent3D {
                width,
                height,
                depth: 1,
            },
        };
        unsafe {
            (self.device_fns.cmd_copy_image_to_buffer)(
                buffer,
                image,
                VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                dst,
                1,
                &region,
            );
        }
    }

    /// Record a render pass that clears `pass` and draws the triangle.
    pub fn cmd_draw_triangle(
        &self,
        buffer: VkCommandBuffer,
        pass: VkRenderPass,
        framebuffer: VkFramebuffer,
        width: u32,
        height: u32,
        clear: [f32; 4],
    ) {
        let clear_value = VkClearValue {
            color: VkClearColorValue { float32: clear },
        };
        let info = VkRenderPassBeginInfo {
            render_pass: pass,
            framebuffer,
            render_area: VkRect2D {
                offset: VkOffset2D { x: 0, y: 0 },
                extent: VkExtent2D { width, height },
            },
            clear_value_count: 1,
            p_clear_values: &clear_value,
            ..VkRenderPassBeginInfo::default()
        };
        unsafe {
            (self.device_fns.cmd_begin_render_pass)(buffer, &info, VK_SUBPASS_CONTENTS_INLINE);
        }
    }

    /// End the active render pass.
    pub fn cmd_end_render_pass(&self, buffer: VkCommandBuffer) {
        unsafe { (self.device_fns.cmd_end_render_pass)(buffer) }
    }

    /// Record a pipeline bind.
    pub fn cmd_bind_pipeline(&self, buffer: VkCommandBuffer, bind_point: u32, pipeline: VkPipeline) {
        unsafe { (self.device_fns.cmd_bind_pipeline)(buffer, bind_point, pipeline) }
    }

    /// Record a descriptor set bind.
    pub fn cmd_bind_descriptor_set(
        &self,
        buffer: VkCommandBuffer,
        bind_point: u32,
        layout: VkPipelineLayout,
        set: VkDescriptorSet,
    ) {
        unsafe {
            (self.device_fns.cmd_bind_descriptor_sets)(
                buffer,
                bind_point,
                layout,
                0,
                1,
                &set,
                0,
                std::ptr::null(),
            );
        }
    }

    /// Record push constants.
    pub fn cmd_push_constants(
        &self,
        buffer: VkCommandBuffer,
        layout: VkPipelineLayout,
        stage: u32,
        bytes: &[u8],
    ) {
        unsafe {
            (self.device_fns.cmd_push_constants)(
                buffer,
                layout,
                stage,
                0,
                bytes.len() as u32,
                bytes.as_ptr().cast(),
            );
        }
    }

    /// Record a compute dispatch.
    pub fn cmd_dispatch(&self, buffer: VkCommandBuffer, groups: u32) {
        unsafe { (self.device_fns.cmd_dispatch)(buffer, groups, 1, 1) }
    }

    /// Record a three-vertex draw.
    pub fn cmd_draw(&self, buffer: VkCommandBuffer) {
        unsafe { (self.device_fns.cmd_draw)(buffer, 3, 1, 0, 0) }
    }
}

impl DeviceFns {
    fn load(get: PFN_vkGetDeviceProcAddr, device: VkDevice) -> Result<Self, DriverError> {
        Ok(Self {
            get_device_queue: load_fn!(get, device, "vkGetDeviceQueue", GetDeviceQueue),
            destroy_device: load_fn!(get, device, "vkDestroyDevice", DestroyDevice),
            device_wait_idle: load_fn!(get, device, "vkDeviceWaitIdle", DeviceWaitIdle),
            create_command_pool: load_fn!(get, device, "vkCreateCommandPool", CreateCommandPool),
            destroy_command_pool: load_fn!(get, device, "vkDestroyCommandPool", DestroyCommandPool),
            allocate_command_buffers: load_fn!(get, device, "vkAllocateCommandBuffers", AllocateCommandBuffers),
            free_command_buffers: load_fn!(get, device, "vkFreeCommandBuffers", FreeCommandBuffers),
            begin_command_buffer: load_fn!(get, device, "vkBeginCommandBuffer", BeginCommandBuffer),
            end_command_buffer: load_fn!(get, device, "vkEndCommandBuffer", EndCommandBuffer),
            queue_submit: load_fn!(get, device, "vkQueueSubmit", QueueSubmit),
            queue_wait_idle: load_fn!(get, device, "vkQueueWaitIdle", QueueWaitIdle),
            create_buffer: load_fn!(get, device, "vkCreateBuffer", CreateBuffer),
            destroy_buffer: load_fn!(get, device, "vkDestroyBuffer", DestroyBuffer),
            get_buffer_memory_requirements: load_fn!(get, device, "vkGetBufferMemoryRequirements", GetBufferMemoryRequirements),
            allocate_memory: load_fn!(get, device, "vkAllocateMemory", AllocateMemory),
            free_memory: load_fn!(get, device, "vkFreeMemory", FreeMemory),
            bind_buffer_memory: load_fn!(get, device, "vkBindBufferMemory", BindBufferMemory),
            map_memory: load_fn!(get, device, "vkMapMemory", MapMemory),
            unmap_memory: load_fn!(get, device, "vkUnmapMemory", UnmapMemory),
            create_image: load_fn!(get, device, "vkCreateImage", CreateImage),
            destroy_image: load_fn!(get, device, "vkDestroyImage", DestroyImage),
            get_image_memory_requirements: load_fn!(get, device, "vkGetImageMemoryRequirements", GetImageMemoryRequirements),
            bind_image_memory: load_fn!(get, device, "vkBindImageMemory", BindImageMemory),
            create_image_view: load_fn!(get, device, "vkCreateImageView", CreateImageView),
            destroy_image_view: load_fn!(get, device, "vkDestroyImageView", DestroyImageView),
            create_render_pass: load_fn!(get, device, "vkCreateRenderPass", CreateRenderPass),
            destroy_render_pass: load_fn!(get, device, "vkDestroyRenderPass", DestroyRenderPass),
            create_framebuffer: load_fn!(get, device, "vkCreateFramebuffer", CreateFramebuffer),
            destroy_framebuffer: load_fn!(get, device, "vkDestroyFramebuffer", DestroyFramebuffer),
            create_shader_module: load_fn!(get, device, "vkCreateShaderModule", CreateShaderModule),
            destroy_shader_module: load_fn!(get, device, "vkDestroyShaderModule", DestroyShaderModule),
            create_pipeline_layout: load_fn!(get, device, "vkCreatePipelineLayout", CreatePipelineLayout),
            destroy_pipeline_layout: load_fn!(get, device, "vkDestroyPipelineLayout", DestroyPipelineLayout),
            create_descriptor_set_layout: load_fn!(get, device, "vkCreateDescriptorSetLayout", CreateDescriptorSetLayout),
            destroy_descriptor_set_layout: load_fn!(get, device, "vkDestroyDescriptorSetLayout", DestroyDescriptorSetLayout),
            create_descriptor_pool: load_fn!(get, device, "vkCreateDescriptorPool", CreateDescriptorPool),
            destroy_descriptor_pool: load_fn!(get, device, "vkDestroyDescriptorPool", DestroyDescriptorPool),
            allocate_descriptor_sets: load_fn!(get, device, "vkAllocateDescriptorSets", AllocateDescriptorSets),
            update_descriptor_sets: load_fn!(get, device, "vkUpdateDescriptorSets", UpdateDescriptorSets),
            create_compute_pipelines: load_fn!(get, device, "vkCreateComputePipelines", CreateComputePipelines),
            create_graphics_pipelines: load_fn!(get, device, "vkCreateGraphicsPipelines", CreateGraphicsPipelines),
            destroy_pipeline: load_fn!(get, device, "vkDestroyPipeline", DestroyPipeline),
            cmd_pipeline_barrier: load_fn!(get, device, "vkCmdPipelineBarrier", CmdPipelineBarrier),
            cmd_clear_color_image: load_fn!(get, device, "vkCmdClearColorImage", CmdClearColorImage),
            cmd_copy_buffer: load_fn!(get, device, "vkCmdCopyBuffer", CmdCopyBuffer),
            cmd_copy_image_to_buffer: load_fn!(get, device, "vkCmdCopyImageToBuffer", CmdCopyImageToBuffer),
            cmd_begin_render_pass: load_fn!(get, device, "vkCmdBeginRenderPass", CmdBeginRenderPass),
            cmd_end_render_pass: load_fn!(get, device, "vkCmdEndRenderPass", CmdEndRenderPass),
            cmd_bind_pipeline: load_fn!(get, device, "vkCmdBindPipeline", CmdBindPipeline),
            cmd_bind_descriptor_sets: load_fn!(get, device, "vkCmdBindDescriptorSets", CmdBindDescriptorSets),
            cmd_push_constants: load_fn!(get, device, "vkCmdPushConstants", CmdPushConstants),
            cmd_dispatch: load_fn!(get, device, "vkCmdDispatch", CmdDispatch),
            cmd_draw: load_fn!(get, device, "vkCmdDraw", CmdDraw),
        })
    }
}

unsafe fn enumerate_physical_devices(
    instance: VkInstance,
    fns: &InstanceFns,
) -> Result<Vec<VkPhysicalDevice>, DriverError> {
    unsafe {
        let mut count = 0u32;
        check(
            (fns.enumerate_physical_devices)(instance, &mut count, std::ptr::null_mut()),
            "vkEnumeratePhysicalDevices",
        )?;
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut devices = vec![std::ptr::null_mut(); count as usize];
        check(
            (fns.enumerate_physical_devices)(instance, &mut count, devices.as_mut_ptr()),
            "vkEnumeratePhysicalDevices",
        )?;
        devices.truncate(count as usize);
        Ok(devices)
    }
}

unsafe fn pick_queue_family(
    _instance: VkInstance,
    fns: &InstanceFns,
    physicals: &[VkPhysicalDevice],
) -> Result<(VkPhysicalDevice, u32, bool), DriverError> {
    unsafe {
        for device in physicals {
            let mut count = 0u32;
            (fns.get_physical_device_queue_family_properties)(*device, &mut count, std::ptr::null_mut());
            let mut families = vec![VkQueueFamilyProperties::default(); count as usize];
            (fns.get_physical_device_queue_family_properties)(*device, &mut count, families.as_mut_ptr());
            families.truncate(count as usize);
            if let Some(index) = families
                .iter()
                .position(|family| family.queue_flags & VK_QUEUE_GRAPHICS_BIT != 0 && family.queue_count > 0)
            {
                return Ok((*device, index as u32, true));
            }
            if let Some(index) = families
                .iter()
                .position(|family| family.queue_flags & VK_QUEUE_COMPUTE_BIT != 0 && family.queue_count > 0)
            {
                return Ok((*device, index as u32, false));
            }
        }
        Err(DriverError::new(-1, "no Vulkan compute/graphics queue"))
    }
}

impl Drop for VulkanDriver {
    fn drop(&mut self) {
        unsafe {
            let _ = (self.device_fns.device_wait_idle)(self.device);
            (self.device_fns.destroy_device)(self.device, std::ptr::null());
            (self.instance_fns.destroy_instance)(self.instance, std::ptr::null());
        }
    }
}

// Vulkan handles are owned by this driver and used from the scheduler thread.
unsafe impl Send for VulkanDriver {}
unsafe impl Sync for VulkanDriver {}
