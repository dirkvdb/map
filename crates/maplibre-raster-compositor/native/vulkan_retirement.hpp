#pragma once
#include <memory>
#include <vulkan/vulkan.h>
namespace map_raster {
// These functions require the callback-scoped native Context from the bridge.
uint64_t storage_buffer_limit();
VkExtent2D attachment_extent();
void retire_frame(std::shared_ptr<void>);
} // namespace map_raster
