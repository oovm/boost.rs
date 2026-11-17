//! Minimal Vulkan ABI types for the built-in compute and raster paths.
//!
//! Only the Vulkan 1.0/1.1 core entry points the backend actually uses are
//! declared here. Handles, structs and enum values follow the Vulkan headers.

use std::ffi::c_void;

/// Vulkan result code.
pub type VkResult = i32;
/// Operation completed successfully.
pub const VK_SUCCESS: VkResult = 0;
/// A partial result was returned.
pub const VK_INCOMPLETE: VkResult = 5;

/// Dispatchable instance handle.
pub type VkInstance = *mut c_void;
/// Dispatchable physical device handle.
pub type VkPhysicalDevice = *mut c_void;
/// Dispatchable logical device handle.
pub type VkDevice = *mut c_void;
/// Dispatchable queue handle.
pub type VkQueue = *mut c_void;
/// Dispatchable command buffer handle.
pub type VkCommandBuffer = *mut c_void;
/// Non-dispatchable command pool handle.
pub type VkCommandPool = u64;
/// Non-dispatchable buffer handle.
pub type VkBuffer = u64;
/// Non-dispatchable device memory handle.
pub type VkDeviceMemory = u64;
/// Non-dispatchable image handle.
pub type VkImage = u64;
/// Non-dispatchable image view handle.
pub type VkImageView = u64;
/// Non-dispatchable render pass handle.
pub type VkRenderPass = u64;
/// Non-dispatchable framebuffer handle.
pub type VkFramebuffer = u64;
/// Non-dispatchable shader module handle.
pub type VkShaderModule = u64;
/// Non-dispatchable pipeline layout handle.
pub type VkPipelineLayout = u64;
/// Non-dispatchable pipeline handle.
pub type VkPipeline = u64;
/// Non-dispatchable descriptor set layout handle.
pub type VkDescriptorSetLayout = u64;
/// Non-dispatchable descriptor pool handle.
pub type VkDescriptorPool = u64;
/// Non-dispatchable descriptor set handle.
pub type VkDescriptorSet = u64;

/// Allocation callback pointer (`None` for the default allocator).
pub type VkAllocationCallbacks = c_void;

/// 32-bit boolean used across the Vulkan ABI.
pub type VkBool32 = u32;
/// Logical device queue flag: graphics.
pub const VK_QUEUE_GRAPHICS_BIT: u32 = 0x0000_0001;
/// Logical device queue flag: compute.
pub const VK_QUEUE_COMPUTE_BIT: u32 = 0x0000_0002;

/// Memory flag: device local.
pub const VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT: u32 = 0x0000_0001;
/// Memory flag: host visible.
pub const VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT: u32 = 0x0000_0002;
/// Memory flag: host coherent.
pub const VK_MEMORY_PROPERTY_HOST_COHERENT_BIT: u32 = 0x0000_0004;

/// Buffer usage: transfer source.
pub const VK_BUFFER_USAGE_TRANSFER_SRC_BIT: u32 = 0x0000_0001;
/// Buffer usage: transfer destination.
pub const VK_BUFFER_USAGE_TRANSFER_DST_BIT: u32 = 0x0000_0002;
/// Buffer usage: storage buffer.
pub const VK_BUFFER_USAGE_STORAGE_BUFFER_BIT: u32 = 0x0000_0020;

/// Image usage: transfer source.
pub const VK_IMAGE_USAGE_TRANSFER_SRC_BIT: u32 = 0x0000_0001;
/// Image usage: transfer destination.
pub const VK_IMAGE_USAGE_TRANSFER_DST_BIT: u32 = 0x0000_0002;
/// Image usage: colour attachment.
pub const VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT: u32 = 0x0000_0010;

/// `VK_FORMAT_R8G8B8A8_UNORM`.
pub const VK_FORMAT_R8G8B8A8_UNORM: u32 = 37;
/// `VK_IMAGE_TYPE_2D`.
pub const VK_IMAGE_TYPE_2D: u32 = 1;
/// `VK_IMAGE_VIEW_TYPE_2D`.
pub const VK_IMAGE_VIEW_TYPE_2D: u32 = 1;
/// `VK_IMAGE_TILING_OPTIMAL`.
pub const VK_IMAGE_TILING_OPTIMAL: u32 = 0;
/// `VK_SAMPLE_COUNT_1_BIT`.
pub const VK_SAMPLE_COUNT_1_BIT: u32 = 1;
/// `VK_SHARING_MODE_EXCLUSIVE`.
pub const VK_SHARING_MODE_EXCLUSIVE: u32 = 0;
/// `VK_IMAGE_ASPECT_COLOR_BIT`.
pub const VK_IMAGE_ASPECT_COLOR_BIT: u32 = 1;
/// `VK_IMAGE_LAYOUT_UNDEFINED`.
pub const VK_IMAGE_LAYOUT_UNDEFINED: u32 = 0;
/// `VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL`.
pub const VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL: u32 = 2;
/// `VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL`.
pub const VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL: u32 = 6;
/// `VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL`.
pub const VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL: u32 = 7;
/// `VK_ATTACHMENT_LOAD_OP_CLEAR`.
pub const VK_ATTACHMENT_LOAD_OP_CLEAR: u32 = 1;
/// `VK_ATTACHMENT_LOAD_OP_DONT_CARE`.
pub const VK_ATTACHMENT_LOAD_OP_DONT_CARE: u32 = 2;
/// `VK_ATTACHMENT_STORE_OP_STORE`.
pub const VK_ATTACHMENT_STORE_OP_STORE: u32 = 0;
/// `VK_ATTACHMENT_STORE_OP_DONT_CARE`.
pub const VK_ATTACHMENT_STORE_OP_DONT_CARE: u32 = 1;
/// `VK_PIPELINE_BIND_POINT_GRAPHICS`.
pub const VK_PIPELINE_BIND_POINT_GRAPHICS: u32 = 0;
/// `VK_PIPELINE_BIND_POINT_COMPUTE`.
pub const VK_PIPELINE_BIND_POINT_COMPUTE: u32 = 1;
/// `VK_SUBPASS_CONTENTS_INLINE`.
pub const VK_SUBPASS_CONTENTS_INLINE: u32 = 0;
/// `VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT`.
pub const VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT: u32 = 0x0000_0001;
/// `VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT`.
pub const VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT: u32 = 0x0000_0400;
/// `VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT`.
pub const VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT: u32 = 0x0000_0800;
/// `VK_PIPELINE_STAGE_TRANSFER_BIT`.
pub const VK_PIPELINE_STAGE_TRANSFER_BIT: u32 = 0x0000_1000;
/// `VK_PIPELINE_STAGE_ALL_COMMANDS_BIT`.
pub const VK_PIPELINE_STAGE_ALL_COMMANDS_BIT: u32 = 0x0001_0000;
/// `VK_ACCESS_SHADER_READ_BIT`.
pub const VK_ACCESS_SHADER_READ_BIT: u32 = 0x0000_0020;
/// `VK_ACCESS_SHADER_WRITE_BIT`.
pub const VK_ACCESS_SHADER_WRITE_BIT: u32 = 0x0000_0040;
/// `VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT`.
pub const VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT: u32 = 0x0000_0100;
/// `VK_ACCESS_TRANSFER_READ_BIT`.
pub const VK_ACCESS_TRANSFER_READ_BIT: u32 = 0x0000_0800;
/// `VK_ACCESS_TRANSFER_WRITE_BIT`.
pub const VK_ACCESS_TRANSFER_WRITE_BIT: u32 = 0x0000_1000;
/// `VK_ACCESS_MEMORY_WRITE_BIT`.
pub const VK_ACCESS_MEMORY_WRITE_BIT: u32 = 0x0001_0000;
/// `VK_DESCRIPTOR_TYPE_STORAGE_BUFFER`.
pub const VK_DESCRIPTOR_TYPE_STORAGE_BUFFER: u32 = 7;
/// `VK_SHADER_STAGE_VERTEX_BIT`.
pub const VK_SHADER_STAGE_VERTEX_BIT: u32 = 0x0000_0001;
/// `VK_SHADER_STAGE_FRAGMENT_BIT`.
pub const VK_SHADER_STAGE_FRAGMENT_BIT: u32 = 0x0000_0010;
/// `VK_SHADER_STAGE_COMPUTE_BIT`.
pub const VK_SHADER_STAGE_COMPUTE_BIT: u32 = 0x0000_0020;
/// `VK_POLYGON_MODE_FILL`.
pub const VK_POLYGON_MODE_FILL: u32 = 0;
/// `VK_CULL_MODE_NONE`.
pub const VK_CULL_MODE_NONE: u32 = 0;
/// `VK_FRONT_FACE_COUNTER_CLOCKWISE`.
pub const VK_FRONT_FACE_COUNTER_CLOCKWISE: u32 = 0;
/// `VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST`.
pub const VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST: u32 = 3;
/// `VK_COLOR_COMPONENT_RGBA_BIT` mask.
pub const VK_COLOR_COMPONENT_RGBA: u32 = 0xF;
/// `VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT`.
pub const VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT: u32 = 0x0000_0002;
/// `VK_COMMAND_BUFFER_LEVEL_PRIMARY`.
pub const VK_COMMAND_BUFFER_LEVEL_PRIMARY: u32 = 0;
/// `VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT`.
pub const VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT: u32 = 0x0000_0001;
/// `VK_SUBPASS_EXTERNAL`.
pub const VK_SUBPASS_EXTERNAL: u32 = 0xFFFF_FFFF;
/// `VK_WHOLE_SIZE`.
pub const VK_WHOLE_SIZE: u64 = u64::MAX;
/// `VK_STRUCTURE_TYPE_*` values used by this module.
pub mod s_type {
    pub const APPLICATION_INFO: u32 = 0;
    pub const INSTANCE_CREATE_INFO: u32 = 1;
    pub const DEVICE_QUEUE_CREATE_INFO: u32 = 2;
    pub const DEVICE_CREATE_INFO: u32 = 3;
    pub const SUBMIT_INFO: u32 = 4;
    pub const MEMORY_ALLOCATE_INFO: u32 = 5;
    pub const BUFFER_CREATE_INFO: u32 = 12;
    pub const IMAGE_CREATE_INFO: u32 = 14;
    pub const IMAGE_VIEW_CREATE_INFO: u32 = 15;
    pub const SHADER_MODULE_CREATE_INFO: u32 = 16;
    pub const PIPELINE_SHADER_STAGE_CREATE_INFO: u32 = 18;
    pub const PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO: u32 = 19;
    pub const PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO: u32 = 20;
    pub const PIPELINE_VIEWPORT_STATE_CREATE_INFO: u32 = 21;
    pub const PIPELINE_RASTERIZATION_STATE_CREATE_INFO: u32 = 22;
    pub const PIPELINE_MULTISAMPLE_STATE_CREATE_INFO: u32 = 23;
    pub const PIPELINE_COLOR_BLEND_STATE_CREATE_INFO: u32 = 26;
    pub const GRAPHICS_PIPELINE_CREATE_INFO: u32 = 28;
    pub const COMPUTE_PIPELINE_CREATE_INFO: u32 = 29;
    pub const PIPELINE_LAYOUT_CREATE_INFO: u32 = 30;
    pub const DESCRIPTOR_SET_LAYOUT_CREATE_INFO: u32 = 32;
    pub const DESCRIPTOR_POOL_CREATE_INFO: u32 = 33;
    pub const DESCRIPTOR_SET_ALLOCATE_INFO: u32 = 34;
    pub const WRITE_DESCRIPTOR_SET: u32 = 35;
    pub const FRAMEBUFFER_CREATE_INFO: u32 = 37;
    pub const RENDER_PASS_CREATE_INFO: u32 = 38;
    pub const COMMAND_POOL_CREATE_INFO: u32 = 39;
    pub const COMMAND_BUFFER_ALLOCATE_INFO: u32 = 40;
    pub const COMMAND_BUFFER_BEGIN_INFO: u32 = 42;
    pub const RENDER_PASS_BEGIN_INFO: u32 = 43;
    pub const IMAGE_MEMORY_BARRIER: u32 = 45;
    pub const MEMORY_BARRIER: u32 = 46;
}

/// `VkApplicationInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkApplicationInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub p_application_name: *const i8,
    pub application_version: u32,
    pub p_engine_name: *const i8,
    pub engine_version: u32,
    pub api_version: u32,
}

impl Default for VkApplicationInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::APPLICATION_INFO,
            p_next: std::ptr::null(),
            p_application_name: std::ptr::null(),
            application_version: 0,
            p_engine_name: std::ptr::null(),
            engine_version: 0,
            api_version: 0,
        }
    }
}

/// `VkInstanceCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkInstanceCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub p_application_info: *const VkApplicationInfo,
    pub enabled_layer_count: u32,
    pub pp_enabled_layer_names: *const *const i8,
    pub enabled_extension_count: u32,
    pub pp_enabled_extension_names: *const *const i8,
}

impl Default for VkInstanceCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::INSTANCE_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            p_application_info: std::ptr::null(),
            enabled_layer_count: 0,
            pp_enabled_layer_names: std::ptr::null(),
            enabled_extension_count: 0,
            pp_enabled_extension_names: std::ptr::null(),
        }
    }
}

/// `VkExtent3D`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkExtent3D {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
}

/// `VkExtent2D`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkExtent2D {
    pub width: u32,
    pub height: u32,
}

/// `VkOffset2D`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkOffset2D {
    pub x: i32,
    pub y: i32,
}

/// `VkOffset3D`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkOffset3D {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

/// `VkRect2D`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkRect2D {
    pub offset: VkOffset2D,
    pub extent: VkExtent2D,
}

/// `VkPhysicalDeviceFeatures` (all members are `VkBool32`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkPhysicalDeviceFeatures {
    /// Feature bits in declaration order.
    pub bits: [VkBool32; 55],
}

/// Index of `shaderFloat64` inside [`VkPhysicalDeviceFeatures`].
pub const FEATURE_SHADER_FLOAT64: usize = 39;

impl Default for VkPhysicalDeviceFeatures {
    fn default() -> Self {
        Self { bits: [0; 55] }
    }
}

/// `VkDeviceQueueCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkDeviceQueueCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub queue_family_index: u32,
    pub queue_count: u32,
    pub p_queue_priorities: *const f32,
}

impl Default for VkDeviceQueueCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::DEVICE_QUEUE_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            queue_family_index: 0,
            queue_count: 0,
            p_queue_priorities: std::ptr::null(),
        }
    }
}

/// `VkDeviceCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkDeviceCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub queue_create_info_count: u32,
    pub p_queue_create_infos: *const VkDeviceQueueCreateInfo,
    pub enabled_layer_count: u32,
    pub pp_enabled_layer_names: *const *const i8,
    pub enabled_extension_count: u32,
    pub pp_enabled_extension_names: *const *const i8,
    pub p_enabled_features: *const VkPhysicalDeviceFeatures,
}

impl Default for VkDeviceCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::DEVICE_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            queue_create_info_count: 0,
            p_queue_create_infos: std::ptr::null(),
            enabled_layer_count: 0,
            pp_enabled_layer_names: std::ptr::null(),
            enabled_extension_count: 0,
            pp_enabled_extension_names: std::ptr::null(),
            p_enabled_features: std::ptr::null(),
        }
    }
}

/// `VkQueueFamilyProperties`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkQueueFamilyProperties {
    pub queue_flags: u32,
    pub queue_count: u32,
    pub timestamp_valid_bits: u32,
    pub min_image_transfer_granularity: VkExtent3D,
}

/// `VkMemoryType`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkMemoryType {
    pub property_flags: u32,
    pub heap_index: u32,
}

/// `VkMemoryHeap`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkMemoryHeap {
    pub size: u64,
    pub flags: u32,
}

/// `VkPhysicalDeviceMemoryProperties`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkPhysicalDeviceMemoryProperties {
    pub memory_type_count: u32,
    pub memory_types: [VkMemoryType; 32],
    pub memory_heap_count: u32,
    pub memory_heaps: [VkMemoryHeap; 16],
}

impl Default for VkPhysicalDeviceMemoryProperties {
    fn default() -> Self {
        Self {
            memory_type_count: 0,
            memory_types: [VkMemoryType::default(); 32],
            memory_heap_count: 0,
            memory_heaps: [VkMemoryHeap::default(); 16],
        }
    }
}

/// `VkBufferCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkBufferCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub size: u64,
    pub usage: u32,
    pub sharing_mode: u32,
    pub queue_family_index_count: u32,
    pub p_queue_family_indices: *const u32,
}

impl Default for VkBufferCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::BUFFER_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            size: 0,
            usage: 0,
            sharing_mode: VK_SHARING_MODE_EXCLUSIVE,
            queue_family_index_count: 0,
            p_queue_family_indices: std::ptr::null(),
        }
    }
}

/// `VkMemoryRequirements`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkMemoryRequirements {
    pub size: u64,
    pub alignment: u64,
    pub memory_type_bits: u32,
}

/// `VkMemoryAllocateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkMemoryAllocateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub allocation_size: u64,
    pub memory_type_index: u32,
}

impl Default for VkMemoryAllocateInfo {
    fn default() -> Self {
        Self { s_type: s_type::MEMORY_ALLOCATE_INFO, p_next: std::ptr::null(), allocation_size: 0, memory_type_index: 0 }
    }
}

/// `VkImageCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkImageCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub image_type: u32,
    pub format: u32,
    pub extent: VkExtent3D,
    pub mip_levels: u32,
    pub array_layers: u32,
    pub samples: u32,
    pub tiling: u32,
    pub usage: u32,
    pub sharing_mode: u32,
    pub queue_family_index_count: u32,
    pub p_queue_family_indices: *const u32,
    pub initial_layout: u32,
}

impl Default for VkImageCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::IMAGE_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            image_type: VK_IMAGE_TYPE_2D,
            format: VK_FORMAT_R8G8B8A8_UNORM,
            extent: VkExtent3D::default(),
            mip_levels: 1,
            array_layers: 1,
            samples: VK_SAMPLE_COUNT_1_BIT,
            tiling: VK_IMAGE_TILING_OPTIMAL,
            usage: 0,
            sharing_mode: VK_SHARING_MODE_EXCLUSIVE,
            queue_family_index_count: 0,
            p_queue_family_indices: std::ptr::null(),
            initial_layout: VK_IMAGE_LAYOUT_UNDEFINED,
        }
    }
}

/// `VkImageSubresourceRange`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkImageSubresourceRange {
    pub aspect_mask: u32,
    pub base_mip_level: u32,
    pub level_count: u32,
    pub base_array_layer: u32,
    pub layer_count: u32,
}

/// `VkComponentMapping`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkComponentMapping {
    pub r: u32,
    pub g: u32,
    pub b: u32,
    pub a: u32,
}

/// `VkImageViewCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkImageViewCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub image: VkImage,
    pub view_type: u32,
    pub format: u32,
    pub components: VkComponentMapping,
    pub subresource_range: VkImageSubresourceRange,
}

impl Default for VkImageViewCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::IMAGE_VIEW_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            image: 0,
            view_type: VK_IMAGE_VIEW_TYPE_2D,
            format: VK_FORMAT_R8G8B8A8_UNORM,
            components: VkComponentMapping::default(),
            subresource_range: VkImageSubresourceRange::default(),
        }
    }
}

/// `VkAttachmentDescription`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkAttachmentDescription {
    pub flags: u32,
    pub format: u32,
    pub samples: u32,
    pub load_op: u32,
    pub store_op: u32,
    pub stencil_load_op: u32,
    pub stencil_store_op: u32,
    pub initial_layout: u32,
    pub final_layout: u32,
}

/// `VkAttachmentReference`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkAttachmentReference {
    pub attachment: u32,
    pub layout: u32,
}

/// `VkSubpassDescription`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkSubpassDescription {
    pub flags: u32,
    pub pipeline_bind_point: u32,
    pub input_attachment_count: u32,
    pub p_input_attachments: *const VkAttachmentReference,
    pub color_attachment_count: u32,
    pub p_color_attachments: *const VkAttachmentReference,
    pub p_resolve_attachments: *const VkAttachmentReference,
    pub p_depth_stencil_attachment: *const VkAttachmentReference,
    pub preserve_attachment_count: u32,
    pub p_preserve_attachments: *const u32,
}

impl Default for VkSubpassDescription {
    fn default() -> Self {
        Self {
            flags: 0,
            pipeline_bind_point: VK_PIPELINE_BIND_POINT_GRAPHICS,
            input_attachment_count: 0,
            p_input_attachments: std::ptr::null(),
            color_attachment_count: 0,
            p_color_attachments: std::ptr::null(),
            p_resolve_attachments: std::ptr::null(),
            p_depth_stencil_attachment: std::ptr::null(),
            preserve_attachment_count: 0,
            p_preserve_attachments: std::ptr::null(),
        }
    }
}

/// `VkSubpassDependency`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkSubpassDependency {
    pub src_subpass: u32,
    pub dst_subpass: u32,
    pub src_stage_mask: u32,
    pub dst_stage_mask: u32,
    pub src_access_mask: u32,
    pub dst_access_mask: u32,
    pub dependency_flags: u32,
}

/// `VkRenderPassCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkRenderPassCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub attachment_count: u32,
    pub p_attachments: *const VkAttachmentDescription,
    pub subpass_count: u32,
    pub p_subpasses: *const VkSubpassDescription,
    pub dependency_count: u32,
    pub p_dependencies: *const VkSubpassDependency,
}

impl Default for VkRenderPassCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::RENDER_PASS_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            attachment_count: 0,
            p_attachments: std::ptr::null(),
            subpass_count: 0,
            p_subpasses: std::ptr::null(),
            dependency_count: 0,
            p_dependencies: std::ptr::null(),
        }
    }
}

/// `VkFramebufferCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkFramebufferCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub render_pass: VkRenderPass,
    pub attachment_count: u32,
    pub p_attachments: *const VkImageView,
    pub width: u32,
    pub height: u32,
    pub layers: u32,
}

impl Default for VkFramebufferCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::FRAMEBUFFER_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            render_pass: 0,
            attachment_count: 0,
            p_attachments: std::ptr::null(),
            width: 0,
            height: 0,
            layers: 1,
        }
    }
}

/// `VkShaderModuleCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkShaderModuleCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub code_size: usize,
    pub p_code: *const u32,
}

impl Default for VkShaderModuleCreateInfo {
    fn default() -> Self {
        Self { s_type: s_type::SHADER_MODULE_CREATE_INFO, p_next: std::ptr::null(), flags: 0, code_size: 0, p_code: std::ptr::null() }
    }
}

/// `VkPipelineShaderStageCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkPipelineShaderStageCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub stage: u32,
    pub module: VkShaderModule,
    pub p_name: *const i8,
    pub p_specialization_info: *const c_void,
}

impl Default for VkPipelineShaderStageCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::PIPELINE_SHADER_STAGE_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            stage: 0,
            module: 0,
            p_name: std::ptr::null(),
            p_specialization_info: std::ptr::null(),
        }
    }
}

/// `VkPipelineVertexInputStateCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkPipelineVertexInputStateCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub vertex_binding_description_count: u32,
    pub p_vertex_binding_descriptions: *const c_void,
    pub vertex_attribute_description_count: u32,
    pub p_vertex_attribute_descriptions: *const c_void,
}

impl Default for VkPipelineVertexInputStateCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            vertex_binding_description_count: 0,
            p_vertex_binding_descriptions: std::ptr::null(),
            vertex_attribute_description_count: 0,
            p_vertex_attribute_descriptions: std::ptr::null(),
        }
    }
}

/// `VkPipelineInputAssemblyStateCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkPipelineInputAssemblyStateCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub topology: u32,
    pub primitive_restart_enable: VkBool32,
}

impl Default for VkPipelineInputAssemblyStateCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            topology: VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST,
            primitive_restart_enable: 0,
        }
    }
}

/// `VkViewport`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkViewport {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub min_depth: f32,
    pub max_depth: f32,
}

/// `VkPipelineViewportStateCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkPipelineViewportStateCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub viewport_count: u32,
    pub p_viewports: *const VkViewport,
    pub scissor_count: u32,
    pub p_scissors: *const VkRect2D,
}

impl Default for VkPipelineViewportStateCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::PIPELINE_VIEWPORT_STATE_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            viewport_count: 0,
            p_viewports: std::ptr::null(),
            scissor_count: 0,
            p_scissors: std::ptr::null(),
        }
    }
}

/// `VkPipelineRasterizationStateCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkPipelineRasterizationStateCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub depth_clamp_enable: VkBool32,
    pub rasterizer_discard_enable: VkBool32,
    pub polygon_mode: u32,
    pub cull_mode: u32,
    pub front_face: u32,
    pub depth_bias_enable: VkBool32,
    pub depth_bias_constant_factor: f32,
    pub depth_bias_clamp: f32,
    pub depth_bias_slope_factor: f32,
    pub line_width: f32,
}

impl Default for VkPipelineRasterizationStateCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            depth_clamp_enable: 0,
            rasterizer_discard_enable: 0,
            polygon_mode: VK_POLYGON_MODE_FILL,
            cull_mode: VK_CULL_MODE_NONE,
            front_face: VK_FRONT_FACE_COUNTER_CLOCKWISE,
            depth_bias_enable: 0,
            depth_bias_constant_factor: 0.0,
            depth_bias_clamp: 0.0,
            depth_bias_slope_factor: 0.0,
            line_width: 1.0,
        }
    }
}

/// `VkPipelineMultisampleStateCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkPipelineMultisampleStateCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub rasterization_samples: u32,
    pub sample_shading_enable: VkBool32,
    pub min_sample_shading: f32,
    pub p_sample_mask: *const u32,
    pub alpha_to_coverage_enable: VkBool32,
    pub alpha_to_one_enable: VkBool32,
}

impl Default for VkPipelineMultisampleStateCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            rasterization_samples: VK_SAMPLE_COUNT_1_BIT,
            sample_shading_enable: 0,
            min_sample_shading: 0.0,
            p_sample_mask: std::ptr::null(),
            alpha_to_coverage_enable: 0,
            alpha_to_one_enable: 0,
        }
    }
}

/// `VkPipelineColorBlendAttachmentState`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkPipelineColorBlendAttachmentState {
    pub blend_enable: VkBool32,
    pub src_color_blend_factor: u32,
    pub dst_color_blend_factor: u32,
    pub color_blend_op: u32,
    pub src_alpha_blend_factor: u32,
    pub dst_alpha_blend_factor: u32,
    pub alpha_blend_op: u32,
    pub color_write_mask: u32,
}

/// `VkPipelineColorBlendStateCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkPipelineColorBlendStateCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub logic_op_enable: VkBool32,
    pub logic_op: u32,
    pub attachment_count: u32,
    pub p_attachments: *const VkPipelineColorBlendAttachmentState,
    pub blend_constants: [f32; 4],
}

impl Default for VkPipelineColorBlendStateCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            logic_op_enable: 0,
            logic_op: 0,
            attachment_count: 0,
            p_attachments: std::ptr::null(),
            blend_constants: [0.0; 4],
        }
    }
}

/// `VkPipelineLayoutCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkPipelineLayoutCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub set_layout_count: u32,
    pub p_set_layouts: *const VkDescriptorSetLayout,
    pub push_constant_range_count: u32,
    pub p_push_constant_ranges: *const VkPushConstantRange,
}

impl Default for VkPipelineLayoutCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::PIPELINE_LAYOUT_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            set_layout_count: 0,
            p_set_layouts: std::ptr::null(),
            push_constant_range_count: 0,
            p_push_constant_ranges: std::ptr::null(),
        }
    }
}

/// `VkPushConstantRange`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkPushConstantRange {
    pub stage_flags: u32,
    pub offset: u32,
    pub size: u32,
}

/// `VkComputePipelineCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkComputePipelineCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub stage: VkPipelineShaderStageCreateInfo,
    pub layout: VkPipelineLayout,
    pub base_pipeline_handle: VkPipeline,
    pub base_pipeline_index: i32,
}

impl Default for VkComputePipelineCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::COMPUTE_PIPELINE_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            stage: VkPipelineShaderStageCreateInfo::default(),
            layout: 0,
            base_pipeline_handle: 0,
            base_pipeline_index: -1,
        }
    }
}

/// `VkGraphicsPipelineCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkGraphicsPipelineCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub stage_count: u32,
    pub p_stages: *const VkPipelineShaderStageCreateInfo,
    pub p_vertex_input_state: *const VkPipelineVertexInputStateCreateInfo,
    pub p_input_assembly_state: *const VkPipelineInputAssemblyStateCreateInfo,
    pub p_tessellation_state: *const c_void,
    pub p_viewport_state: *const VkPipelineViewportStateCreateInfo,
    pub p_rasterization_state: *const VkPipelineRasterizationStateCreateInfo,
    pub p_multisample_state: *const VkPipelineMultisampleStateCreateInfo,
    pub p_depth_stencil_state: *const c_void,
    pub p_color_blend_state: *const VkPipelineColorBlendStateCreateInfo,
    pub p_dynamic_state: *const c_void,
    pub layout: VkPipelineLayout,
    pub render_pass: VkRenderPass,
    pub subpass: u32,
    pub base_pipeline_handle: VkPipeline,
    pub base_pipeline_index: i32,
}

impl Default for VkGraphicsPipelineCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::GRAPHICS_PIPELINE_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            stage_count: 0,
            p_stages: std::ptr::null(),
            p_vertex_input_state: std::ptr::null(),
            p_input_assembly_state: std::ptr::null(),
            p_tessellation_state: std::ptr::null(),
            p_viewport_state: std::ptr::null(),
            p_rasterization_state: std::ptr::null(),
            p_multisample_state: std::ptr::null(),
            p_depth_stencil_state: std::ptr::null(),
            p_color_blend_state: std::ptr::null(),
            p_dynamic_state: std::ptr::null(),
            layout: 0,
            render_pass: 0,
            subpass: 0,
            base_pipeline_handle: 0,
            base_pipeline_index: -1,
        }
    }
}

/// `VkDescriptorSetLayoutBinding`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkDescriptorSetLayoutBinding {
    pub binding: u32,
    pub descriptor_type: u32,
    pub descriptor_count: u32,
    pub stage_flags: u32,
    pub p_immutable_samplers: *const c_void,
}

impl Default for VkDescriptorSetLayoutBinding {
    fn default() -> Self {
        Self { binding: 0, descriptor_type: 0, descriptor_count: 0, stage_flags: 0, p_immutable_samplers: std::ptr::null() }
    }
}

/// `VkDescriptorSetLayoutCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkDescriptorSetLayoutCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub binding_count: u32,
    pub p_bindings: *const VkDescriptorSetLayoutBinding,
}

impl Default for VkDescriptorSetLayoutCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            binding_count: 0,
            p_bindings: std::ptr::null(),
        }
    }
}

/// `VkDescriptorPoolSize`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkDescriptorPoolSize {
    pub ty: u32,
    pub descriptor_count: u32,
}

/// `VkDescriptorPoolCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkDescriptorPoolCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub max_sets: u32,
    pub pool_size_count: u32,
    pub p_pool_sizes: *const VkDescriptorPoolSize,
}

impl Default for VkDescriptorPoolCreateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::DESCRIPTOR_POOL_CREATE_INFO,
            p_next: std::ptr::null(),
            flags: 0,
            max_sets: 0,
            pool_size_count: 0,
            p_pool_sizes: std::ptr::null(),
        }
    }
}

/// `VkDescriptorSetAllocateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkDescriptorSetAllocateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub descriptor_pool: VkDescriptorPool,
    pub descriptor_set_count: u32,
    pub p_set_layouts: *const VkDescriptorSetLayout,
}

impl Default for VkDescriptorSetAllocateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::DESCRIPTOR_SET_ALLOCATE_INFO,
            p_next: std::ptr::null(),
            descriptor_pool: 0,
            descriptor_set_count: 0,
            p_set_layouts: std::ptr::null(),
        }
    }
}

/// `VkDescriptorBufferInfo`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkDescriptorBufferInfo {
    pub buffer: VkBuffer,
    pub offset: u64,
    pub range: u64,
}

/// `VkWriteDescriptorSet`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkWriteDescriptorSet {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub dst_set: VkDescriptorSet,
    pub dst_binding: u32,
    pub dst_array_element: u32,
    pub descriptor_count: u32,
    pub descriptor_type: u32,
    pub p_image_info: *const c_void,
    pub p_buffer_info: *const VkDescriptorBufferInfo,
    pub p_texel_buffer_view: *const c_void,
}

impl Default for VkWriteDescriptorSet {
    fn default() -> Self {
        Self {
            s_type: s_type::WRITE_DESCRIPTOR_SET,
            p_next: std::ptr::null(),
            dst_set: 0,
            dst_binding: 0,
            dst_array_element: 0,
            descriptor_count: 0,
            descriptor_type: 0,
            p_image_info: std::ptr::null(),
            p_buffer_info: std::ptr::null(),
            p_texel_buffer_view: std::ptr::null(),
        }
    }
}

/// `VkBufferCopy`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkBufferCopy {
    pub src_offset: u64,
    pub dst_offset: u64,
    pub size: u64,
}

/// `VkImageSubresourceLayers`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkImageSubresourceLayers {
    pub aspect_mask: u32,
    pub mip_level: u32,
    pub base_array_layer: u32,
    pub layer_count: u32,
}

/// `VkBufferImageCopy`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkBufferImageCopy {
    pub buffer_offset: u64,
    pub buffer_row_length: u32,
    pub buffer_image_height: u32,
    pub image_subresource: VkImageSubresourceLayers,
    pub image_offset: VkOffset3D,
    pub image_extent: VkExtent3D,
}

/// `VkImageMemoryBarrier`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkImageMemoryBarrier {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub src_access_mask: u32,
    pub dst_access_mask: u32,
    pub old_layout: u32,
    pub new_layout: u32,
    pub src_queue_family_index: u32,
    pub dst_queue_family_index: u32,
    pub image: VkImage,
    pub subresource_range: VkImageSubresourceRange,
}

impl Default for VkImageMemoryBarrier {
    fn default() -> Self {
        Self {
            s_type: s_type::IMAGE_MEMORY_BARRIER,
            p_next: std::ptr::null(),
            src_access_mask: 0,
            dst_access_mask: 0,
            old_layout: VK_IMAGE_LAYOUT_UNDEFINED,
            new_layout: VK_IMAGE_LAYOUT_UNDEFINED,
            src_queue_family_index: u32::MAX,
            dst_queue_family_index: u32::MAX,
            image: 0,
            subresource_range: VkImageSubresourceRange::default(),
        }
    }
}

/// `VkMemoryBarrier`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkMemoryBarrier {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub src_access_mask: u32,
    pub dst_access_mask: u32,
}

impl Default for VkMemoryBarrier {
    fn default() -> Self {
        Self { s_type: s_type::MEMORY_BARRIER, p_next: std::ptr::null(), src_access_mask: 0, dst_access_mask: 0 }
    }
}

/// `VkCommandPoolCreateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkCommandPoolCreateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub queue_family_index: u32,
}

impl Default for VkCommandPoolCreateInfo {
    fn default() -> Self {
        Self { s_type: s_type::COMMAND_POOL_CREATE_INFO, p_next: std::ptr::null(), flags: 0, queue_family_index: 0 }
    }
}

/// `VkCommandBufferAllocateInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkCommandBufferAllocateInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub command_pool: VkCommandPool,
    pub level: u32,
    pub command_buffer_count: u32,
}

impl Default for VkCommandBufferAllocateInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::COMMAND_BUFFER_ALLOCATE_INFO,
            p_next: std::ptr::null(),
            command_pool: 0,
            level: VK_COMMAND_BUFFER_LEVEL_PRIMARY,
            command_buffer_count: 0,
        }
    }
}

/// `VkCommandBufferBeginInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkCommandBufferBeginInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub flags: u32,
    pub p_inheritance_info: *const c_void,
}

impl Default for VkCommandBufferBeginInfo {
    fn default() -> Self {
        Self { s_type: s_type::COMMAND_BUFFER_BEGIN_INFO, p_next: std::ptr::null(), flags: 0, p_inheritance_info: std::ptr::null() }
    }
}

/// `VkSubmitInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkSubmitInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub wait_semaphore_count: u32,
    pub p_wait_semaphores: *const u64,
    pub p_wait_dst_stage_mask: *const u32,
    pub command_buffer_count: u32,
    pub p_command_buffers: *const VkCommandBuffer,
    pub signal_semaphore_count: u32,
    pub p_signal_semaphores: *const u64,
}

impl Default for VkSubmitInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::SUBMIT_INFO,
            p_next: std::ptr::null(),
            wait_semaphore_count: 0,
            p_wait_semaphores: std::ptr::null(),
            p_wait_dst_stage_mask: std::ptr::null(),
            command_buffer_count: 0,
            p_command_buffers: std::ptr::null(),
            signal_semaphore_count: 0,
            p_signal_semaphores: std::ptr::null(),
        }
    }
}

/// `VkClearColorValue` (float variant is the only one used here).
#[repr(C)]
#[derive(Clone, Copy)]
pub union VkClearColorValue {
    /// Floating-point channel values.
    pub float32: [f32; 4],
    /// Signed integer channel values.
    pub int32: [i32; 4],
    /// Unsigned integer channel values.
    pub uint32: [u32; 4],
}

impl Default for VkClearColorValue {
    fn default() -> Self {
        Self { float32: [0.0; 4] }
    }
}

/// `VkClearValue` (only the colour member is used).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VkClearValue {
    /// Colour attachment clear value.
    pub color: VkClearColorValue,
}

/// `VkRenderPassBeginInfo`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkRenderPassBeginInfo {
    pub s_type: u32,
    pub p_next: *const c_void,
    pub render_pass: VkRenderPass,
    pub framebuffer: VkFramebuffer,
    pub render_area: VkRect2D,
    pub clear_value_count: u32,
    pub p_clear_values: *const VkClearValue,
}

impl Default for VkRenderPassBeginInfo {
    fn default() -> Self {
        Self {
            s_type: s_type::RENDER_PASS_BEGIN_INFO,
            p_next: std::ptr::null(),
            render_pass: 0,
            framebuffer: 0,
            render_area: VkRect2D::default(),
            clear_value_count: 0,
            p_clear_values: std::ptr::null(),
        }
    }
}

/// `vkGetInstanceProcAddr` signature.
pub type PFN_vkGetInstanceProcAddr = unsafe extern "C" fn(VkInstance, *const i8) -> *mut c_void;
/// `vkGetDeviceProcAddr` signature.
pub type PFN_vkGetDeviceProcAddr = unsafe extern "C" fn(VkDevice, *const i8) -> *mut c_void;

/// Driver-owned error.
#[derive(Debug)]
pub struct DriverError {
    /// Vulkan result code or `-1` for loader failures.
    pub code: VkResult,
    /// Operation name.
    pub message: String,
}

impl DriverError {
    /// Build an error from a Vulkan result code and operation name.
    pub fn new(code: VkResult, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Vulkan error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for DriverError {}

/// Abort on a non-success Vulkan result.
pub fn check(code: VkResult, api: &str) -> Result<VkResult, DriverError> {
    if code == VK_SUCCESS || code == VK_INCOMPLETE || code >= 0 { Ok(code) } else { Err(DriverError::new(code, api)) }
}
