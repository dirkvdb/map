#define VK_NO_PROTOTYPES
#include "backend.hpp"
#include "spirv.hpp"
#include "vulkan_retirement.hpp"
#include <atomic>
#include <bit>
#include <cmath>
#include <cstring>
#include <string>
#include <type_traits>
#include <utility>

namespace map_raster {
namespace {
template <class Handle> Handle from_bits(uint64_t bits) {
  if constexpr (std::is_pointer_v<Handle>)
    return reinterpret_cast<Handle>(uintptr_t(bits));
  else
    return static_cast<Handle>(bits);
}
void check(VkResult result, const char *operation) {
  if (result != VK_SUCCESS)
    throw std::runtime_error(std::string(operation) + " failed: VkResult " +
                             std::to_string(result));
}
#define MAP_VK_FUNCTIONS(X)                                                    \
  X(CreateBuffer)                                                              \
  X(DestroyBuffer) X(GetBufferMemoryRequirements) X(AllocateMemory)            \
      X(FreeMemory) X(BindBufferMemory) X(MapMemory) X(UnmapMemory)            \
          X(FlushMappedMemoryRanges) X(CreateDescriptorSetLayout)              \
              X(DestroyDescriptorSetLayout) X(CreateDescriptorPool)            \
                  X(DestroyDescriptorPool) X(AllocateDescriptorSets)           \
                      X(UpdateDescriptorSets) X(CreatePipelineLayout)          \
                          X(DestroyPipelineLayout) X(CreateShaderModule)       \
                              X(DestroyShaderModule)                           \
                                  X(CreateGraphicsPipelines)                   \
                                      X(DestroyPipeline) X(CmdBindPipeline)    \
                                          X(CmdBindDescriptorSets)             \
                                              X(CmdPushConstants) X(CmdDraw)   \
                                                  X(CmdSetViewport)            \
                                                      X(CmdSetScissor)

struct Device {
  VkDevice handle;
  VkPhysicalDeviceMemoryProperties memory{};
  std::atomic<bool> abandoned{false};
#define DECLARE(name) PFN_vk##name name = nullptr;
  MAP_VK_FUNCTIONS(DECLARE)
#undef DECLARE
  explicit Device(const mln_custom_graphics_context &graphics)
      : handle(static_cast<VkDevice>(graphics.vk_device)) {
    if (!handle || !graphics.vk_physical_device ||
        !graphics.vk_get_device_proc_addr ||
        !graphics.vk_get_physical_device_memory_properties)
      throw std::runtime_error(
          "Vulkan device/physical device/dispatch missing");
#define LOAD(name)                                                             \
  name = reinterpret_cast<PFN_vk##name>(                                       \
      graphics.vk_get_device_proc_addr(handle, "vk" #name));                   \
  if (!name)                                                                   \
    throw std::runtime_error("Vulkan procedure missing: vk" #name);
    MAP_VK_FUNCTIONS(LOAD)
#undef LOAD
    graphics.vk_get_physical_device_memory_properties(
        static_cast<VkPhysicalDevice>(graphics.vk_physical_device), &memory);
  }
  uint32_t memory_type(uint32_t bits) const {
    for (uint32_t i = 0; i < memory.memoryTypeCount; ++i) {
      if ((bits & (1u << i)) && (memory.memoryTypes[i].propertyFlags &
                                 VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT))
        return i;
    }
    throw std::runtime_error("no host-visible Vulkan storage-buffer memory");
  }
};

struct Buffer {
  std::shared_ptr<Device> device;
  VkBuffer handle = VK_NULL_HANDLE;
  VkDeviceMemory memory = VK_NULL_HANDLE;
  VkDeviceSize size = 0;
  explicit Buffer(std::shared_ptr<Device> value) : device(std::move(value)) {}
  ~Buffer() {
    if (device->abandoned)
      return;
    if (handle)
      device->DestroyBuffer(device->handle, handle, nullptr);
    if (memory)
      device->FreeMemory(device->handle, memory, nullptr);
  }
  void upload(const void *source, VkDeviceSize bytes) {
    size = bytes;
    VkBufferCreateInfo info{VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO};
    info.size = bytes;
    info.usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT;
    info.sharingMode = VK_SHARING_MODE_EXCLUSIVE;
    check(device->CreateBuffer(device->handle, &info, nullptr, &handle),
          "vkCreateBuffer");
    VkMemoryRequirements requirements{};
    device->GetBufferMemoryRequirements(device->handle, handle, &requirements);
    VkMemoryAllocateInfo allocation{VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO};
    allocation.allocationSize = requirements.size;
    allocation.memoryTypeIndex =
        device->memory_type(requirements.memoryTypeBits);
    check(device->AllocateMemory(device->handle, &allocation, nullptr, &memory),
          "vkAllocateMemory");
    check(device->BindBufferMemory(device->handle, handle, memory, 0),
          "vkBindBufferMemory");
    void *mapped = nullptr;
    check(
        device->MapMemory(device->handle, memory, 0, VK_WHOLE_SIZE, 0, &mapped),
        "vkMapMemory");
    std::memcpy(mapped, source, static_cast<size_t>(bytes));
    VkMappedMemoryRange range{VK_STRUCTURE_TYPE_MAPPED_MEMORY_RANGE};
    range.memory = memory;
    range.size = VK_WHOLE_SIZE;
    const auto result =
        device->FlushMappedMemoryRanges(device->handle, 1, &range);
    device->UnmapMemory(device->handle, memory);
    check(result, "vkFlushMappedMemoryRanges");
  }
};

struct Pipeline {
  std::shared_ptr<Device> device;
  VkDescriptorSetLayout descriptors = VK_NULL_HANDLE;
  VkPipelineLayout layout = VK_NULL_HANDLE;
  VkPipeline pipeline = VK_NULL_HANDLE;
  uint64_t pass_bits;
  bool extruded, depth_test;
  Pipeline(std::shared_ptr<Device> value, uint64_t pass, bool raised, bool test)
      : device(std::move(value)), pass_bits(pass), extruded(raised),
        depth_test(test) {}
  ~Pipeline() {
    if (device->abandoned)
      return;
    if (pipeline)
      device->DestroyPipeline(device->handle, pipeline, nullptr);
    if (layout)
      device->DestroyPipelineLayout(device->handle, layout, nullptr);
    if (descriptors)
      device->DestroyDescriptorSetLayout(device->handle, descriptors, nullptr);
  }
  void create_layout() {
    const VkDescriptorSetLayoutBinding bindings[] = {
        {0, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_FRAGMENT_BIT,
         nullptr},
        {1, VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 1, VK_SHADER_STAGE_FRAGMENT_BIT,
         nullptr}};
    VkDescriptorSetLayoutCreateInfo descriptorInfo{
        VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO};
    descriptorInfo.bindingCount = 2;
    descriptorInfo.pBindings = bindings;
    check(device->CreateDescriptorSetLayout(device->handle, &descriptorInfo,
                                            nullptr, &descriptors),
          "descriptor layout");
    const VkPushConstantRange push{VK_SHADER_STAGE_FRAGMENT_BIT, 0, 16};
    VkPipelineLayoutCreateInfo layoutInfo{
        VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO};
    layoutInfo.setLayoutCount = 1;
    layoutInfo.pSetLayouts = &descriptors;
    layoutInfo.pushConstantRangeCount = 1;
    layoutInfo.pPushConstantRanges = &push;
    check(device->CreatePipelineLayout(device->handle, &layoutInfo, nullptr,
                                       &layout),
          "pipeline layout");
  }
  void create_pipeline(VkRenderPass pass);
};

struct Frame {
  std::shared_ptr<Device> device;
  // Frames share immutable pipeline state, never their upload buffers. Fence
  // retirement keeps both alive until every draw using them has completed.
  std::shared_ptr<Pipeline> pipeline;
  Buffer color, depth;
  VkDescriptorPool pool = VK_NULL_HANDLE;
  VkDescriptorSet set = VK_NULL_HANDLE;
  uint32_t width, height;
  explicit Frame(std::shared_ptr<Device> value, const map_surface &surface)
      : device(std::move(value)), color(device), depth(device),
        width(surface.width), height(surface.height) {}
  ~Frame() {
    if (!device->abandoned && pool)
      device->DestroyDescriptorPool(device->handle, pool, nullptr);
  }
  void upload(const map_surface &surface) {
    const VkDeviceSize bytes = VkDeviceSize(width) * height * 4;
    color.upload(surface.color, bytes);
    depth.upload(surface.depth, bytes);
    create_descriptors();
  }
  void create_descriptors() {
    const VkDescriptorPoolSize poolSize{VK_DESCRIPTOR_TYPE_STORAGE_BUFFER, 2};
    VkDescriptorPoolCreateInfo poolInfo{
        VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO};
    poolInfo.maxSets = 1;
    poolInfo.poolSizeCount = 1;
    poolInfo.pPoolSizes = &poolSize;
    check(
        device->CreateDescriptorPool(device->handle, &poolInfo, nullptr, &pool),
        "descriptor pool");
    VkDescriptorSetAllocateInfo allocation{
        VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO};
    allocation.descriptorPool = pool;
    allocation.descriptorSetCount = 1;
    allocation.pSetLayouts = &pipeline->descriptors;
    check(device->AllocateDescriptorSets(device->handle, &allocation, &set),
          "descriptor allocation");
    const VkDescriptorBufferInfo buffers[] = {{color.handle, 0, color.size},
                                              {depth.handle, 0, depth.size}};
    VkWriteDescriptorSet writes[2]{};
    for (uint32_t i = 0; i < 2; ++i) {
      writes[i].sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET;
      writes[i].dstSet = set;
      writes[i].dstBinding = i;
      writes[i].descriptorCount = 1;
      writes[i].descriptorType = VK_DESCRIPTOR_TYPE_STORAGE_BUFFER;
      writes[i].pBufferInfo = &buffers[i];
    }
    device->UpdateDescriptorSets(device->handle, 2, writes, 0, nullptr);
  }
};

struct Shader {
  Device &device;
  VkShaderModule handle = VK_NULL_HANDLE;
  Shader(Device &value, const uint32_t *code, size_t size) : device(value) {
    VkShaderModuleCreateInfo info{VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO};
    info.codeSize = size;
    info.pCode = code;
    check(device.CreateShaderModule(device.handle, &info, nullptr, &handle),
          "shader module");
  }
  ~Shader() {
    if (handle)
      device.DestroyShaderModule(device.handle, handle, nullptr);
  }
};

void Pipeline::create_pipeline(VkRenderPass pass) {
  Shader vertex(*device, fullscreen_vert, sizeof(fullscreen_vert));
  Shader fragment(*device, composite_frag, sizeof(composite_frag));
  VkPipelineShaderStageCreateInfo stages[2]{};
  for (auto &stage : stages) {
    stage.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO;
    stage.pName = "main";
  }
  stages[0].stage = VK_SHADER_STAGE_VERTEX_BIT;
  stages[0].module = vertex.handle;
  stages[1].stage = VK_SHADER_STAGE_FRAGMENT_BIT;
  stages[1].module = fragment.handle;
  VkPipelineVertexInputStateCreateInfo input{
      VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO};
  VkPipelineInputAssemblyStateCreateInfo assembly{
      VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO};
  assembly.topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST;
  VkPipelineViewportStateCreateInfo viewport{
      VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO};
  viewport.viewportCount = viewport.scissorCount = 1;
  VkPipelineRasterizationStateCreateInfo raster{
      VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO};
  raster.polygonMode = VK_POLYGON_MODE_FILL;
  raster.cullMode = VK_CULL_MODE_NONE;
  raster.frontFace = VK_FRONT_FACE_COUNTER_CLOCKWISE;
  raster.lineWidth = 1;
  VkPipelineMultisampleStateCreateInfo multisample{
      VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO};
  multisample.rasterizationSamples = VK_SAMPLE_COUNT_1_BIT;
  VkPipelineDepthStencilStateCreateInfo depthState{
      VK_STRUCTURE_TYPE_PIPELINE_DEPTH_STENCIL_STATE_CREATE_INFO};
  depthState.depthTestEnable = extruded || depth_test;
  depthState.depthWriteEnable = extruded;
  depthState.depthCompareOp = VK_COMPARE_OP_LESS_OR_EQUAL;
  VkPipelineColorBlendAttachmentState attachment{};
  attachment.blendEnable = VK_TRUE;
  attachment.srcColorBlendFactor = attachment.srcAlphaBlendFactor =
      VK_BLEND_FACTOR_ONE;
  attachment.dstColorBlendFactor = attachment.dstAlphaBlendFactor =
      VK_BLEND_FACTOR_ONE_MINUS_SRC_ALPHA;
  attachment.colorBlendOp = attachment.alphaBlendOp = VK_BLEND_OP_ADD;
  attachment.colorWriteMask = 15;
  VkPipelineColorBlendStateCreateInfo blend{
      VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO};
  blend.attachmentCount = 1;
  blend.pAttachments = &attachment;
  const VkDynamicState states[] = {VK_DYNAMIC_STATE_VIEWPORT,
                                   VK_DYNAMIC_STATE_SCISSOR};
  VkPipelineDynamicStateCreateInfo dynamic{
      VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO};
  dynamic.dynamicStateCount = 2;
  dynamic.pDynamicStates = states;
  VkGraphicsPipelineCreateInfo info{
      VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO};
  info.stageCount = 2;
  info.pStages = stages;
  info.pVertexInputState = &input;
  info.pInputAssemblyState = &assembly;
  info.pViewportState = &viewport;
  info.pRasterizationState = &raster;
  info.pMultisampleState = &multisample;
  info.pDepthStencilState = &depthState;
  info.pColorBlendState = &blend;
  info.pDynamicState = &dynamic;
  info.layout = layout;
  info.renderPass = pass;
  check(device->CreateGraphicsPipelines(device->handle, VK_NULL_HANDLE, 1,
                                        &info, nullptr, &pipeline),
        "graphics pipeline");
}

class VulkanBackend final : public Backend {
  std::shared_ptr<Device> device;
  std::shared_ptr<Frame> frame;

  std::shared_ptr<Pipeline> pipeline_for(uint64_t pass, bool extruded,
                                       bool depth_test) {
    if (frame && frame->pipeline->pass_bits == pass &&
        frame->pipeline->extruded == extruded &&
        frame->pipeline->depth_test == depth_test)
      return frame->pipeline;
    auto next = std::make_shared<Pipeline>(device, pass, extruded, depth_test);
    next->create_layout();
    next->create_pipeline(from_bits<VkRenderPass>(pass));
    return next;
  }

public:
  explicit VulkanBackend(const mln_custom_graphics_context &graphics)
      : device(std::make_shared<Device>(graphics)) {}
  void prepare(const mln_custom_frame_parameters &parameters,
               const map_surface &surface) override {

    if (device->handle != parameters.graphics.vk_device)
      throw std::runtime_error("Vulkan device changed");
    if constexpr (std::endian::native != std::endian::little)
      throw std::runtime_error(
          "Vulkan RGBA buffer packing requires a little-endian host");
    const auto pixels = uint64_t(surface.width) * surface.height;
    if (pixels > UINT32_MAX)
      throw std::runtime_error("surface exceeds shader uint indexing");
    const auto bytes = pixels * 4;
    if (!surface.width || !surface.height || !surface.color || !surface.depth ||
        bytes > storage_buffer_limit())
      throw std::runtime_error(
          "surface exceeds Vulkan maxStorageBufferRange or is empty");
    if (!parameters.graphics.vk_render_pass)
      throw std::runtime_error(
          "pre_render lacks compatible Vulkan render pass");
    auto next = std::make_shared<Frame>(device, surface);
    next->pipeline = pipeline_for(parameters.graphics.vk_render_pass,
                                  surface.extruded, parameters.depth_test);
    next->upload(surface);
    retire();
    frame = std::move(next);
  }
  void render(const mln_custom_frame_parameters &parameters) override {
    const auto &graphics = parameters.graphics;
    if (!frame || !graphics.vk_command_buffer || !graphics.vk_render_pass ||
        device->handle != graphics.vk_device)
      throw std::runtime_error(
          "missing prepared surface or active Vulkan pass/device");
    if (frame->pipeline->depth_test != parameters.depth_test ||
        frame->pipeline->pass_bits != graphics.vk_render_pass)
      throw std::runtime_error("Vulkan pass/depth mode changed after prepare");
    const auto command =
        static_cast<VkCommandBuffer>(graphics.vk_command_buffer);
    struct Push {
      uint32_t width, height;
      float cosine, sine;
    };
    const Push push{
        frame->width, frame->height,
        std::cos(graphics.vk_screen_pre_rotation_radians_clockwise),
        std::sin(graphics.vk_screen_pre_rotation_radians_clockwise)};
    const auto extent = attachment_extent();
    const VkViewport viewport{0, 0, float(extent.width), float(extent.height),
                              0, 1};
    const VkRect2D scissor{{0, 0}, extent};
    device->CmdSetViewport(command, 0, 1, &viewport);
    device->CmdSetScissor(command, 0, 1, &scissor);
    device->CmdBindPipeline(command, VK_PIPELINE_BIND_POINT_GRAPHICS,
                             frame->pipeline->pipeline);
    device->CmdBindDescriptorSets(command, VK_PIPELINE_BIND_POINT_GRAPHICS,
                                   frame->pipeline->layout, 0, 1, &frame->set, 0,
                                   nullptr);
    device->CmdPushConstants(command, frame->pipeline->layout,
                             VK_SHADER_STAGE_FRAGMENT_BIT, 0, sizeof(push),
                             &push);
    device->CmdDraw(command, 3, 1, 0, 0);
  }
  void retire() {
    if (!frame)
      return;
    // Capture before mutating ownership: allocation failure leaves the frame
    // live.
    retire_frame(frame);
    frame.reset();
  }
  void deinitialize() override { retire(); }
  void abandon() noexcept override {
    device->abandoned = true;
    frame.reset();
  }
};
} // namespace
std::unique_ptr<Backend>
make_backend(const mln_custom_graphics_context &graphics) {
  if (graphics.backend != MLN_CUSTOM_BACKEND_VULKAN)
    throw std::runtime_error("this native archive supports Vulkan only");
  return std::make_unique<VulkanBackend>(graphics);
}
} // namespace map_raster
