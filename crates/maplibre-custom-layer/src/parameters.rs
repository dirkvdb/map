use std::ffi::{c_char, c_void};

/// Function returned by Vulkan's device procedure lookup.
pub type VulkanFunction = Option<unsafe extern "system" fn()>;
/// ABI-compatible with `PFN_vkGetDeviceProcAddr` (opaque `VkDevice`).
pub type GetDeviceProcAddr = Option<unsafe extern "system" fn(*mut c_void, *const c_char) -> VulkanFunction>;

/// ABI-compatible with `PFN_vkGetPhysicalDeviceMemoryProperties`.
/// The output pointer must name a writable native `VkPhysicalDeviceMemoryProperties`.
pub type GetPhysicalDeviceMemoryProperties = Option<unsafe extern "system" fn(*mut c_void, *mut c_void)>;

/// No backend-specific handles are available.
pub const BACKEND_OTHER: u32 = 0;
/// Vulkan graphics context.
pub const BACKEND_VULKAN: u32 = 1;
/// Metal graphics context.
pub const BACKEND_METAL: u32 = 2;

/// Borrowed native graphics objects, valid only during the current callback.
///
/// Do not destroy native objects or submit/end the native pass or command buffer.
/// Pre-render is outside the pass; its handles may be null. Vulkan initialization
/// has a device and physical device but no pass/buffer. Metal initialization has
/// no device; obtain it in pre-render/render. All raw-handle use remains unsafe.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct GraphicsContext {
    /// One of the `BACKEND_*` constants.
    pub backend: u32,
    /// ABI padding, zero.
    pub reserved: u32,
    /// `VkDevice` dispatchable handle.
    pub vk_device: *mut c_void,
    /// `VkPhysicalDevice` dispatchable handle, retained from initialization.
    pub vk_physical_device: *mut c_void,
    /// `VkRenderPass` non-dispatchable handle bits.
    pub vk_render_pass: u64,
    /// `VkCommandBuffer` dispatchable handle.
    pub vk_command_buffer: *mut c_void,
    /// Device procedure loader; use the Vulkan system calling convention.
    pub vk_get_device_proc_addr: GetDeviceProcAddr,
    /// Instance-level memory query unavailable through the device-only loader.
    pub vk_get_physical_device_memory_properties: GetPhysicalDeviceMemoryProperties,
    /// Clockwise clip-space pre-rotation required by the native render target.
    pub vk_screen_pre_rotation_radians_clockwise: f32,
    /// ABI padding, zero.
    pub reserved2: u32,
    /// Borrowed `id<MTLDevice>` / metal-cpp device.
    pub metal_device: *mut c_void,
    /// Borrowed active `id<MTLRenderCommandEncoder>`.
    pub metal_encoder: *mut c_void,
    /// Borrowed `id<MTLCommandBuffer>`.
    pub metal_command_buffer: *mut c_void,
    /// Borrowed `MTLRenderPassDescriptor*`.
    pub metal_render_pass_descriptor: *mut c_void,
}

/// Native camera/projection values, copied verbatim from `CustomLayerHost`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FrameParameters {
    /// Native logical viewport width, not necessarily attachment pixel width.
    pub width: f64,
    /// Native logical viewport height.
    pub height: f64,
    /// Camera center latitude in degrees.
    pub latitude: f64,
    /// Camera center longitude in degrees.
    pub longitude: f64,
    /// Native zoom.
    pub zoom: f64,
    /// Bearing in degrees (native convention).
    pub bearing: f64,
    /// Pitch in radians.
    pub pitch: f64,
    /// Field of view in radians.
    pub field_of_view: f64,
    /// Native standard world projection, near Z = 1 tile unit; tile/model transform not included.
    pub projection_matrix: [f64; 16],
    /// Column-major native near-clipped projection, without conversion.
    pub near_clipped_projection_matrix: [f64; 16],
    /// Native pixel-grid-aligned world projection with flat depth offset applied.
    /// Apply the existing tile/model transform; do not subtract the offset again.
    pub raster_projection_matrix: [f64; 16],
    /// Native custom-layer documentation's default flat clip-space Z: 1.
    pub default_flat_clip_z: f64,
    /// Finite positive clip-Z translation already subtracted from raster matrix `[14]`.
    /// Zero if native flat depth testing is disabled. Not a post-divide NDC offset.
    pub flat_depth_offset: f64,
    /// Native logical-to-physical pixel ratio (attachment dimensions may round).
    pub pixel_ratio: f64,
    /// Whether the native pass tests this flat layer against its depth attachment.
    pub depth_test: bool,
    /// ABI padding, zero.
    pub reserved: [u8; 7],
    /// Callback-scoped native graphics handles.
    pub graphics: GraphicsContext,
}
