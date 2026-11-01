#include "vulkan_retirement.hpp"
#include <mln/vulkan/context.hpp>
#include <mln/vulkan/renderable_resource.hpp>
#include <mln/vulkan/renderer_backend.hpp>
#include <stdexcept>

extern "C" void *mln_custom_layer_vulkan_context() noexcept;
namespace map_raster {
namespace {
mln::vulkan::Context &context() {
  auto *value =
      static_cast<mln::vulkan::Context *>(mln_custom_layer_vulkan_context());
  if (!value)
    throw std::runtime_error(
        "Vulkan compositor requires the active custom-layer retirement scope");
  return *value;
}
} // namespace
uint64_t storage_buffer_limit() {
  return context()
      .getBackend()
      .getDeviceProperties()
      .limits.maxStorageBufferRange;
}
VkExtent2D attachment_extent() {
  const auto &renderable = context().getBackend().getDefaultRenderable();
  const auto &extent =
      renderable.getResource<mln::vulkan::RenderableResource>().getExtent();
  return {extent.width, extent.height};
}
void retire_frame(std::shared_ptr<void> resource) {
  context().enqueueDeletion(
      [resource = std::move(resource)](mln::vulkan::Context &) {});
}
} // namespace map_raster
