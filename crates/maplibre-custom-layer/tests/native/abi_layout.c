/* Compile as C23 and C++20 against the official public headers. */
#include "maplibre_custom_layer.h"
#include <stddef.h>

static_assert(MLN_CUSTOM_LAYER_ABI_VERSION == 2u, "raster frame ABI version");

#if UINTPTR_MAX == UINT64_MAX
static_assert(sizeof(mln_custom_graphics_context) == 96, "graphics ABI");
static_assert(offsetof(mln_custom_graphics_context, vk_get_device_proc_addr) ==
                  40,
              "Vulkan loader ABI");
static_assert(offsetof(mln_custom_graphics_context, metal_device) == 64,
              "Metal ABI");
static_assert(sizeof(mln_custom_frame_parameters) == 576, "frame ABI");
static_assert(offsetof(mln_custom_frame_parameters, raster_projection_matrix) ==
                  320,
              "raster matrix ABI");
static_assert(offsetof(mln_custom_frame_parameters, flat_depth_offset) == 456,
              "depth offset ABI");
static_assert(offsetof(mln_custom_frame_parameters, pixel_ratio) == 464,
              "pixel ratio ABI");
static_assert(offsetof(mln_custom_frame_parameters, depth_test) == 472,
              "depth test ABI");
static_assert(offsetof(mln_custom_frame_parameters, graphics) == 480,
              "frame graphics ABI");
static_assert(sizeof(mln_custom_layer_callbacks) == 64, "callbacks ABI");
#endif

/* No Vulkan headers are required for consumers of the bridge header itself.
 * This optional check verifies the actual PFN signature on Vulkan builds. */
#ifdef MLN_CUSTOM_TEST_VULKAN
#include <vulkan/vulkan.h>
#ifdef __cplusplus
#include <type_traits>
static_assert(std::is_same_v<mln_custom_vk_get_device_proc_addr,
                             PFN_vkGetDeviceProcAddr>);
static_assert(
    std::is_same_v<mln_custom_vk_get_physical_device_memory_properties,
                   PFN_vkGetPhysicalDeviceMemoryProperties>);
#else
static_assert(_Generic((mln_custom_vk_get_device_proc_addr)0,
                  PFN_vkGetDeviceProcAddr: 1,
                  default: 0),
              "Vulkan loader signature ABI");
static_assert(_Generic((mln_custom_vk_get_physical_device_memory_properties)0,
                  PFN_vkGetPhysicalDeviceMemoryProperties: 1,
                  default: 0),
              "Vulkan memory query signature ABI");
#endif
#endif
