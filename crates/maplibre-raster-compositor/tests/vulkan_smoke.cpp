#include "compositor.h"
#include "vulkan_retirement.hpp"
#include <array>
#include <vulkan/vulkan.h>

#include <cmath>
#include <cstdio>
#include <cstring>
#include <stdexcept>
#include <vector>

namespace {
std::vector<std::shared_ptr<void>> retirement;
uint32_t pipeline_creations = 0;
VKAPI_ATTR VkResult VKAPI_CALL counted_create_pipelines(
    VkDevice device, VkPipelineCache cache, uint32_t count,
    const VkGraphicsPipelineCreateInfo *infos,
    const VkAllocationCallbacks *allocator, VkPipeline *pipelines) {
  pipeline_creations += count;
  return vkCreateGraphicsPipelines(device, cache, count, infos, allocator,
                                   pipelines);
}
VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL counted_device_proc(
    VkDevice device, const char *name) {
  if (std::strcmp(name, "vkCreateGraphicsPipelines") == 0)
    return reinterpret_cast<PFN_vkVoidFunction>(counted_create_pipelines);
  return vkGetDeviceProcAddr(device, name);
}
void check(VkResult result) {
  if (result != VK_SUCCESS)
    throw std::runtime_error("Vulkan test result " + std::to_string(result));
}
void require(bool condition) {
  if (!condition)
    throw std::runtime_error("GPU readback/retirement assertion failed");
}
} // namespace
// Test-only stand-in for MapLibre's fence-backed deletion queue. Resources are
// explicitly released only after the harness's real graphics queue completes.
namespace map_raster {
uint64_t storage_buffer_limit() { return 128 * 1024 * 1024; }
VkExtent2D attachment_extent() { return {2, 2}; }
void retire_frame(std::shared_ptr<void> resource) {
  retirement.push_back(std::move(resource));
}
} // namespace map_raster
namespace {
struct GPU {
  VkInstance instance{};
  VkPhysicalDevice physical{};
  VkDevice device{};
  VkQueue queue{};
  uint32_t family{};
  VkPhysicalDeviceMemoryProperties memory{};
  VkCommandPool pool{};
  GPU() {
    VkInstanceCreateInfo info{VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO};
    check(vkCreateInstance(&info, nullptr, &instance));
    uint32_t count = 0;
    check(vkEnumeratePhysicalDevices(instance, &count, nullptr));
    if (!count)
      throw std::runtime_error("no Vulkan physical device");
    std::vector<VkPhysicalDevice> devices(count);
    check(vkEnumeratePhysicalDevices(instance, &count, devices.data()));
    physical = devices.front();
    vkGetPhysicalDeviceQueueFamilyProperties(physical, &count, nullptr);
    std::vector<VkQueueFamilyProperties> families(count);
    vkGetPhysicalDeviceQueueFamilyProperties(physical, &count, families.data());
    while (family < count &&
           !(families[family].queueFlags & VK_QUEUE_GRAPHICS_BIT))
      ++family;
    if (family == count)
      throw std::runtime_error("no graphics queue");
    float priority = 1;
    VkDeviceQueueCreateInfo queueInfo{
        VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO};
    queueInfo.queueFamilyIndex = family;
    queueInfo.queueCount = 1;
    queueInfo.pQueuePriorities = &priority;
    VkDeviceCreateInfo deviceInfo{VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO};
    deviceInfo.queueCreateInfoCount = 1;
    deviceInfo.pQueueCreateInfos = &queueInfo;
    check(vkCreateDevice(physical, &deviceInfo, nullptr, &device));
    vkGetDeviceQueue(device, family, 0, &queue);
    vkGetPhysicalDeviceMemoryProperties(physical, &memory);
    VkCommandPoolCreateInfo poolInfo{
        VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO};
    poolInfo.queueFamilyIndex = family;
    check(vkCreateCommandPool(device, &poolInfo, nullptr, &pool));
  }
  ~GPU() {
    vkDeviceWaitIdle(device);
    retirement.clear();
    vkDestroyCommandPool(device, pool, nullptr);
    vkDestroyDevice(device, nullptr);
    vkDestroyInstance(instance, nullptr);
  }
  uint32_t memory_type(uint32_t bits, VkMemoryPropertyFlags flags) {
    for (uint32_t i = 0; i < memory.memoryTypeCount; ++i)
      if ((bits & (1u << i)) &&
          (memory.memoryTypes[i].propertyFlags & flags) == flags)
        return i;
    throw std::runtime_error("test allocation has no suitable memory");
  }
  mln_custom_frame_parameters parameters() {
    mln_custom_frame_parameters result{};
    result.width = result.height = 2;
    result.pixel_ratio = 1;
    result.graphics.backend = MLN_CUSTOM_BACKEND_VULKAN;
    result.graphics.vk_device = device;
    result.graphics.vk_physical_device = physical;
    result.graphics.vk_get_device_proc_addr = counted_device_proc;
    result.graphics.vk_get_physical_device_memory_properties =
        vkGetPhysicalDeviceMemoryProperties;
    return result;
  }
};
struct Image {
  GPU &gpu;
  VkImage image{};
  VkImageView view{};
  VkDeviceMemory memory{};
  Image(GPU &owner, VkFormat format, VkImageUsageFlags usage,
        VkImageAspectFlags aspect)
      : gpu(owner) {
    VkImageCreateInfo info{VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO};
    info.imageType = VK_IMAGE_TYPE_2D;
    info.format = format;
    info.extent = {2, 2, 1};
    info.mipLevels = info.arrayLayers = 1;
    info.samples = VK_SAMPLE_COUNT_1_BIT;
    info.tiling = VK_IMAGE_TILING_OPTIMAL;
    info.usage = usage | VK_IMAGE_USAGE_TRANSFER_SRC_BIT;
    check(vkCreateImage(gpu.device, &info, nullptr, &image));
    VkMemoryRequirements requirements{};
    vkGetImageMemoryRequirements(gpu.device, image, &requirements);
    VkMemoryAllocateInfo allocation{VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO};
    allocation.allocationSize = requirements.size;
    allocation.memoryTypeIndex =
        gpu.memory_type(requirements.memoryTypeBits, 0);
    check(vkAllocateMemory(gpu.device, &allocation, nullptr, &memory));
    check(vkBindImageMemory(gpu.device, image, memory, 0));
    VkImageViewCreateInfo viewInfo{VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO};
    viewInfo.image = image;
    viewInfo.viewType = VK_IMAGE_VIEW_TYPE_2D;
    viewInfo.format = format;
    viewInfo.subresourceRange = {aspect, 0, 1, 0, 1};
    check(vkCreateImageView(gpu.device, &viewInfo, nullptr, &view));
  }
  ~Image() {
    vkDestroyImageView(gpu.device, view, nullptr);
    vkDestroyImage(gpu.device, image, nullptr);
    vkFreeMemory(gpu.device, memory, nullptr);
  }
};
struct Target {
  GPU &gpu;
  Image color, depth;
  VkRenderPass pass{};
  VkFramebuffer framebuffer{};
  Target(GPU &owner)
      : gpu(owner),
        color(owner, VK_FORMAT_R8G8B8A8_UNORM,
              VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT, VK_IMAGE_ASPECT_COLOR_BIT),
        depth(owner, VK_FORMAT_D32_SFLOAT_S8_UINT,
              VK_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT_BIT,
              VK_IMAGE_ASPECT_DEPTH_BIT | VK_IMAGE_ASPECT_STENCIL_BIT) {
    VkAttachmentDescription attachments[2]{};
    attachments[0].format = VK_FORMAT_R8G8B8A8_UNORM;
    attachments[1].format = VK_FORMAT_D32_SFLOAT_S8_UINT;
    for (auto &attachment : attachments) {
      attachment.samples = VK_SAMPLE_COUNT_1_BIT;
      attachment.loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR;
      attachment.storeOp = VK_ATTACHMENT_STORE_OP_STORE;
      attachment.finalLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
    }
    const VkAttachmentReference colorRef{
        0, VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL};
    const VkAttachmentReference depthRef{
        1, VK_IMAGE_LAYOUT_DEPTH_STENCIL_ATTACHMENT_OPTIMAL};
    VkSubpassDescription subpass{};
    subpass.pipelineBindPoint = VK_PIPELINE_BIND_POINT_GRAPHICS;
    subpass.colorAttachmentCount = 1;
    subpass.pColorAttachments = &colorRef;
    subpass.pDepthStencilAttachment = &depthRef;
    VkRenderPassCreateInfo info{VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO};
    info.attachmentCount = 2;
    info.pAttachments = attachments;
    info.subpassCount = 1;
    info.pSubpasses = &subpass;
    check(vkCreateRenderPass(gpu.device, &info, nullptr, &pass));
    const VkImageView views[] = {color.view, depth.view};
    VkFramebufferCreateInfo framebufferInfo{
        VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO};
    framebufferInfo.renderPass = pass;
    framebufferInfo.attachmentCount = 2;
    framebufferInfo.pAttachments = views;
    framebufferInfo.width = framebufferInfo.height = 2;
    framebufferInfo.layers = 1;
    check(vkCreateFramebuffer(gpu.device, &framebufferInfo, nullptr,
                              &framebuffer));
  }
  ~Target() {
    vkDestroyFramebuffer(gpu.device, framebuffer, nullptr);
    vkDestroyRenderPass(gpu.device, pass, nullptr);
  }
};
struct Case {
  bool extruded, test, rotated = false, clipped = false;
};
size_t source_pixel(size_t pixel, bool rotated) {
  constexpr size_t clockwise[] = {2, 0, 3, 1};
  return rotated ? clockwise[pixel] : pixel;
}
struct Readback {
  GPU &gpu;
  VkBuffer buffer{};
  VkDeviceMemory memory{};
  Readback(GPU &owner) : gpu(owner) {
    VkBufferCreateInfo info{VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO};
    info.size = 32;
    info.usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT;
    check(vkCreateBuffer(gpu.device, &info, nullptr, &buffer));
    VkMemoryRequirements requirements{};
    vkGetBufferMemoryRequirements(gpu.device, buffer, &requirements);
    VkMemoryAllocateInfo allocation{VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO};
    allocation.allocationSize = requirements.size;
    allocation.memoryTypeIndex = gpu.memory_type(
        requirements.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT |
                                         VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
    check(vkAllocateMemory(gpu.device, &allocation, nullptr, &memory));
    check(vkBindBufferMemory(gpu.device, buffer, memory, 0));
  }
  ~Readback() {
    vkDestroyBuffer(gpu.device, buffer, nullptr);
    vkFreeMemory(gpu.device, memory, nullptr);
  }
  void copy(VkCommandBuffer command, Target &target) {
    VkMemoryBarrier barrier{VK_STRUCTURE_TYPE_MEMORY_BARRIER};
    barrier.srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT |
                            VK_ACCESS_DEPTH_STENCIL_ATTACHMENT_WRITE_BIT;
    barrier.dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
    vkCmdPipelineBarrier(command,
                         VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT |
                             VK_PIPELINE_STAGE_LATE_FRAGMENT_TESTS_BIT,
                         VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 1, &barrier, 0,
                         nullptr, 0, nullptr);
    VkBufferImageCopy region{};
    region.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1};
    region.imageExtent = {2, 2, 1};
    vkCmdCopyImageToBuffer(command, target.color.image,
                           VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, buffer, 1,
                           &region);
    region.bufferOffset = 16;
    region.imageSubresource.aspectMask = VK_IMAGE_ASPECT_DEPTH_BIT;
    vkCmdCopyImageToBuffer(command, target.depth.image,
                           VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, buffer, 1,
                           &region);
  }
  void verify(Case mode) {
    void *mapped = nullptr;
    check(vkMapMemory(gpu.device, memory, 0, VK_WHOLE_SIZE, 0, &mapped));
    const auto *rgba = static_cast<const uint8_t *>(mapped);
    const auto *z = reinterpret_cast<const float *>(rgba + 16);
    std::array<uint8_t, 16> expected = {
        128,
        0,
        127,
        255,
        0,
        0,
        255,
        255,
        0,
        uint8_t(mode.test || mode.extruded ? 0 : 255),
        uint8_t(mode.test || mode.extruded ? 255 : 0),
        255,
        255,
        255,
        255,
        255};
    if (mode.clipped) {
      expected[12] = expected[13] = 0;
    }
    for (size_t i = 0; i < expected.size(); ++i)
      require(
          std::abs(int(rgba[i]) -
                   expected[source_pixel(i / 4, mode.rotated) * 4 + i % 4]) <=
          1);
    const std::array<float, 4> expectedDepth = {mode.extruded ? 0.25f : 0.5f,
                                                0.5f, 0.5f, 0.5f};
    for (size_t i = 0; i < 4; ++i)
      require(std::abs(z[i] - expectedDepth[source_pixel(i, mode.rotated)]) <
              0.00001f);
    vkUnmapMemory(gpu.device, memory);
  }
};
void draw_case(GPU &gpu, Case mode) {
  Target target(gpu);
  Readback readback(gpu);
  auto parameters = gpu.parameters();
  parameters.depth_test = mode.test;
  parameters.graphics.vk_screen_pre_rotation_radians_clockwise =
      mode.rotated ? 1.57079632679f : 0;
  parameters.graphics.vk_render_pass =
      uint64_t(reinterpret_cast<uintptr_t>(target.pass));
  const uint8_t color[] = {128, 0,   0, 128, 255, 0,   0,   0,
                           0,   255, 0, 255, 255, 255, 255, 255};
  // Parent GL-to-wgpu depth conversion; native depths are .25, .1, .75, .5.
  float depth[] = {0.625, 0.55, 0.875, 0.75};
  if (mode.clipped)
    depth[3] =
        0.25f; // Native z=-0.5 must discard, even with flat testing disabled.
  const map_surface surface{2, 2, color, depth, mode.extruded};
  auto *compositor = map_compositor_new();
  const auto initial_pipelines = pipeline_creations;
  char error[1024]{};
  if (map_compositor_prepare(compositor, &parameters, &surface, error,
                             sizeof(error)))
    throw std::runtime_error(error);
  VkCommandBufferAllocateInfo allocation{
      VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO};
  allocation.commandPool = gpu.pool;
  allocation.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY;
  allocation.commandBufferCount = 1;
  VkCommandBuffer command{};
  check(vkAllocateCommandBuffers(gpu.device, &allocation, &command));
  VkCommandBufferBeginInfo begin{VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
  check(vkBeginCommandBuffer(command, &begin));
  VkClearValue clear[2]{};
  clear[0].color.float32[2] = clear[0].color.float32[3] = 1;
  clear[1].depthStencil.depth = 0.5;
  VkRenderPassBeginInfo passInfo{VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO};
  passInfo.renderPass = target.pass;
  passInfo.framebuffer = target.framebuffer;
  passInfo.renderArea.extent = {2, 2};
  passInfo.clearValueCount = 2;
  passInfo.pClearValues = clear;
  vkCmdBeginRenderPass(command, &passInfo, VK_SUBPASS_CONTENTS_INLINE);
  parameters.graphics.vk_command_buffer = command;
  if (map_compositor_render(compositor, &parameters, error, sizeof(error)))
    throw std::runtime_error(error);
  vkCmdEndRenderPass(command);
  readback.copy(command, target);
  check(vkEndCommandBuffer(command));
  // Replace prepared data and delete the host BEFORE submission: the previous
  // command must still read its immutable buffers, not the transparent
  // replacement.
  const uint8_t transparent[16]{};
  const map_surface replacement{2, 2, transparent, depth, mode.extruded};
  if (map_compositor_prepare(compositor, &parameters, &replacement, error,
                              sizeof(error)))
    throw std::runtime_error(error);
  require(pipeline_creations == initial_pipelines + 1);
  // A depth-mode change must create new immutable state without invalidating
  // the pipeline referenced by the command recorded above.
  parameters.depth_test = !parameters.depth_test;
  if (map_compositor_prepare(compositor, &parameters, &replacement, error,
                             sizeof(error)))
    throw std::runtime_error(error);
  require(pipeline_creations == initial_pipelines + 2);
  if (map_compositor_deinitialize(compositor, error, sizeof(error)))
    throw std::runtime_error(error);
  map_compositor_delete(compositor);
  require(retirement.size() == 3);
  VkSubmitInfo submit{VK_STRUCTURE_TYPE_SUBMIT_INFO};
  submit.commandBufferCount = 1;
  submit.pCommandBuffers = &command;
  check(vkQueueSubmit(gpu.queue, 1, &submit, VK_NULL_HANDLE));
  check(vkQueueWaitIdle(
      gpu.queue)); // Harness only: compositor never waits/submits.
  readback.verify(mode);
  retirement.clear();
  vkFreeCommandBuffers(gpu.device, gpu.pool, 1, &command);
}
} // namespace
int main() {
  try {
    GPU gpu;
    draw_case(gpu, {true, false});
    draw_case(gpu, {false, true});
    draw_case(gpu, {false, false});
    draw_case(gpu, {true, true, true});
    draw_case(gpu, {false, false, false, true});
    std::puts("Vulkan real GPU smoke: premultiplied blend, discard, "
              "flat/extruded depth, early retirement passed");
    return 0;
  } catch (const std::exception &error) {
    std::fprintf(stderr, "%s\n", error.what());
    return 1;
  }
}
