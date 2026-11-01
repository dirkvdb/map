#ifndef MAPLIBRE_CUSTOM_LAYER_H
#define MAPLIBRE_CUSTOM_LAYER_H

#include <maplibre_native_c.h>
#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define MLN_CUSTOM_LAYER_ABI_VERSION 2u
#define MLN_CUSTOM_BACKEND_OTHER 0u
#define MLN_CUSTOM_BACKEND_VULKAN 1u
#define MLN_CUSTOM_BACKEND_METAL 2u

#if defined(_WIN32)
#define MLN_CUSTOM_VK_CALL __stdcall
#else
#define MLN_CUSTOM_VK_CALL
#endif
/* Exact PFN_vkVoidFunction/PFN_vkGetDeviceProcAddr types without pulling in
 * Vulkan headers on Metal. VkDevice_T is Vulkan's dispatchable opaque type. */
struct VkDevice_T;
struct VkPhysicalDevice_T;
struct VkPhysicalDeviceMemoryProperties;
typedef void(MLN_CUSTOM_VK_CALL *mln_custom_vk_function)(void);
typedef mln_custom_vk_function(
    MLN_CUSTOM_VK_CALL *mln_custom_vk_get_device_proc_addr)(
    struct VkDevice_T *device, const char *name);
typedef void(
    MLN_CUSTOM_VK_CALL *mln_custom_vk_get_physical_device_memory_properties)(
    struct VkPhysicalDevice_T *physical_device,
    struct VkPhysicalDeviceMemoryProperties *properties);

/* All handles are borrowed. Never destroy native-owned objects. Handles and
 * their validity are callback-scoped, including pre_render versus render.
 * Vulkan non-dispatchable handles use uint64_t on every architecture. */
typedef struct mln_custom_graphics_context {
  uint32_t backend;
  uint32_t reserved;
  void *vk_device;
  void *vk_physical_device;
  uint64_t vk_render_pass;
  void *vk_command_buffer;
  mln_custom_vk_get_device_proc_addr vk_get_device_proc_addr;
  mln_custom_vk_get_physical_device_memory_properties
      vk_get_physical_device_memory_properties;
  float vk_screen_pre_rotation_radians_clockwise;
  uint32_t reserved2;
  void *metal_device;
  void *metal_encoder;
  void *metal_command_buffer;
  void *metal_render_pass_descriptor;
} mln_custom_graphics_context;

/* Column-major matrices, copied without conversion. Bearing is degrees;
 * pitch and field_of_view are radians. Dimensions are native logical pixels.
 * projection_matrix/near_clipped_projection_matrix retain native semantics.
 * raster_projection_matrix is the pixel-grid-aligned world projection with
 * finite flat_depth_offset ALREADY subtracted from [14]; apply the existing
 * tile/model transform. This offset is a clip-Z translation, not an NDC bias.
 * depth_test controls native flat depth testing; offset is zero when disabled.
 * pixel_ratio maps logical to physical pixels. default_flat_clip_z is native's
 * documented custom-layer z=1 convention, not a replacement for raster depth.
 */
typedef struct mln_custom_frame_parameters {
  double width;
  double height;
  double latitude;
  double longitude;
  double zoom;
  double bearing;
  double pitch;
  double field_of_view;
  double projection_matrix[16];
  double near_clipped_projection_matrix[16];
  double raster_projection_matrix[16];
  double default_flat_clip_z;
  double flat_depth_offset;
  double pixel_ratio;
  bool depth_test;
  uint8_t reserved[7];
  mln_custom_graphics_context graphics;
} mln_custom_frame_parameters;

/* No callback may unwind, throw, or reenter/wait on the map or driver.
 * Native serializes lifecycle callbacks for this host; release is final, after
 * every reference (style, renderer and pending work) has gone. deinitialize
 * and context_lost are forwarded as native invokes them, never synthesized by
 * the final release. In particular deinitialize may precede initialize.
 * Metal initialize has no handles; acquire the device at pre_render/render.
 * pre_render is outside the active pass: encoder/pass/command buffer can be
 * null there. Only render's command buffer/encoder is for native-pass drawing.
 */
typedef struct mln_custom_layer_callbacks {
  uint32_t abi_version;
  uint32_t size;
  void *user_data;
  void (*initialize)(void *, const mln_custom_graphics_context *);
  void (*pre_render)(void *, const mln_custom_frame_parameters *);
  void (*render)(void *, const mln_custom_frame_parameters *);
  void (*context_lost)(void *);
  void (*deinitialize)(void *);
  void (*release_user_data)(void *);
} mln_custom_layer_callbacks;

/* Uses the FFI's own map command queue. id/before are copied during this call;
 * before.data == NULL means append. Empty/NUL-containing IDs are rejected.
 * Requires all callbacks and a completion with callback + release and no queue.
 * MLN_STATUS_OK transfers callback userdata AND completion userdata to native;
 * queued failure/cancellation releases the host even when never initialized.
 * Non-OK transfers neither and invokes neither callback/release. Acceptance
 * does NOT imply insertion success: inspect the terminal completion result.
 * The successful completion has no value, just status and map generations. */
MLN_API mln_status
mln_custom_layer_add(mln_map map, mln_buffer_view id,
                     const mln_custom_layer_callbacks *callbacks,
                     mln_buffer_view before, const mln_completion *completion,
                     mln_diagnostic *out_diagnostic) MLN_NOEXCEPT;

#ifdef __cplusplus
}
#endif
#endif
