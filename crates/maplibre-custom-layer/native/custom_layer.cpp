#define MLN_BUILDING_C
#include "core_patch_required.hpp"
#include "maplibre_custom_layer.h"

#include "c_api/boundary.hpp"
#include "diagnostics/diagnostics.hpp"
#include "execution/forbid_reentry.hpp"
#include "map/map.hpp"

#include <mln/map/map.hpp>
#include <mln/style/layers/custom_layer.hpp>
#include <mln/style/style.hpp>

#if MLN_RENDER_BACKEND_VULKAN
// Backend macros must precede the lightweight Vulkan parameter headers.
#include <mln/vulkan/context.hpp>
#include <mln/style/layers/vulkan/custom_layer_init_parameters.hpp>
#include <mln/style/layers/vulkan/custom_layer_render_parameters.hpp>
#elif MLN_RENDER_BACKEND_METAL
#include <mln/style/layers/mtl/custom_layer_render_parameters.hpp>
#endif

#include <algorithm>
#include <atomic>
#include <cstring>

#include <memory>
#include <optional>
#include <string>
#include <type_traits>

#if MLN_RENDER_BACKEND_VULKAN
static_assert(std::is_same_v<mln_custom_vk_get_device_proc_addr,
                             PFN_vkGetDeviceProcAddr>);
static_assert(
    std::is_same_v<mln_custom_vk_get_physical_device_memory_properties,
                   PFN_vkGetPhysicalDeviceMemoryProperties>);
#endif

namespace {
thread_local const mln::gfx::Context *callback_context = nullptr;
struct CallbackContextScope {
  const mln::gfx::Context *previous = callback_context;
  explicit CallbackContextScope(const mln::gfx::Context *context) {
    callback_context = context;
  }
  ~CallbackContextScope() { callback_context = previous; }
};
constexpr const char *callback_reentry =
    "custom layer callbacks cannot reenter the FFI";

// A submitting reference survives until acceptance is recorded, even if the
// executor already ran/rejected the command and destroyed its reference.
struct Registration {
  mln_custom_layer_callbacks callbacks;
  std::atomic<bool> accepted{false};

  explicit Registration(const mln_custom_layer_callbacks &value)
      : callbacks(value) {}
  ~Registration() {
    if (accepted.load(std::memory_order_acquire)) {
      mln::core::ForbidReentry scope(callback_reentry);
      callbacks.release_user_data(callbacks.user_data);
    }
  }
};

template <typename Handle> uint64_t non_dispatchable_bits(Handle handle) {
  if constexpr (std::is_pointer_v<Handle>) {
    return static_cast<uint64_t>(reinterpret_cast<uintptr_t>(handle));
  } else {
    return static_cast<uint64_t>(handle);
  }
}

mln_custom_frame_parameters
frame_parameters(const mln::style::CustomLayerRenderParameters &source,
                 mln_custom_graphics_context graphics) {
  mln_custom_frame_parameters result{};
  result.width = source.width;
  result.height = source.height;
  result.latitude = source.latitude;
  result.longitude = source.longitude;
  result.zoom = source.zoom;
  result.bearing = source.bearing;
  result.pitch = source.pitch;
  result.field_of_view = source.fieldOfView;
  std::copy(source.projectionMatrix.begin(), source.projectionMatrix.end(),
            result.projection_matrix);
  std::copy(source.nearClippedProjectionMatrix.begin(),
            source.nearClippedProjectionMatrix.end(),
            result.near_clipped_projection_matrix);
  std::copy(source.rasterProjectionMatrix.begin(),
            source.rasterProjectionMatrix.end(),
            result.raster_projection_matrix);
  result.default_flat_clip_z = 1.0;
  result.flat_depth_offset = source.flatDepthOffset;
  result.pixel_ratio = source.pixelRatio;
  result.depth_test = source.depthTest;
#if MLN_RENDER_BACKEND_VULKAN
  const auto &native =
      static_cast<const mln::style::vulkan::CustomLayerRenderParameters &>(
          source);
  graphics.vk_device = static_cast<VkDevice>(native.device);
  graphics.vk_render_pass =
      non_dispatchable_bits(static_cast<VkRenderPass>(native.renderPass));
  graphics.vk_command_buffer =
      static_cast<VkCommandBuffer>(native.commandBuffer);
  graphics.vk_get_device_proc_addr = native.dispatcher.vkGetDeviceProcAddr;
  graphics.vk_get_physical_device_memory_properties =
      native.dispatcher.vkGetPhysicalDeviceMemoryProperties;
  graphics.vk_screen_pre_rotation_radians_clockwise =
      native.screenPreRotationRadiansClockwise;
#elif MLN_RENDER_BACKEND_METAL
  const auto &native =
      static_cast<const mln::style::mtl::CustomLayerRenderParameters &>(source);
  graphics.metal_encoder = native.encoder.get();
  graphics.metal_command_buffer = native.commandBuffer.get();
  graphics.metal_render_pass_descriptor = native.renderPassDesc.get();
  if (native.commandBuffer)
    graphics.metal_device = native.commandBuffer->device();
#endif
  result.graphics = graphics;
  return result;
}

class Host final : public mln::style::CustomLayerHost {
public:
  explicit Host(std::shared_ptr<Registration> registration)
      : registration_(std::move(registration)) {}

  void
  initialize(const mln::style::CustomLayerInitParameters &source) override {
    graphics_ = {};
#if MLN_RENDER_BACKEND_VULKAN
    const auto &native =
        static_cast<const mln::style::vulkan::CustomLayerInitParameters &>(
            source);
    graphics_.backend = MLN_CUSTOM_BACKEND_VULKAN;
    graphics_.vk_device = static_cast<VkDevice>(native.device);
    graphics_.vk_physical_device =
        static_cast<VkPhysicalDevice>(native.physicalDevice);
    graphics_.vk_get_device_proc_addr = native.dispatcher.vkGetDeviceProcAddr;
    graphics_.vk_get_physical_device_memory_properties =
        native.dispatcher.vkGetPhysicalDeviceMemoryProperties;
#elif MLN_RENDER_BACKEND_METAL
    static_cast<void>(source);
    graphics_.backend = MLN_CUSTOM_BACKEND_METAL;
#else
    static_cast<void>(source);
#endif
    mln::core::ForbidReentry scope(callback_reentry);
    registration_->callbacks.initialize(registration_->callbacks.user_data,
                                        &graphics_);
  }

  void
  preRender(const mln::gfx::Context &context,
            const mln::style::CustomLayerRenderParameters &source) override {
    context_ = &context;
    CallbackContextScope contextScope(context_);
    const auto parameters = frame_parameters(source, graphics_);
    mln::core::ForbidReentry scope(callback_reentry);
    registration_->callbacks.pre_render(registration_->callbacks.user_data,
                                        &parameters);
  }

  void render(const mln::style::CustomLayerRenderParameters &source) override {
    CallbackContextScope contextScope(context_);
    const auto parameters = frame_parameters(source, graphics_);
    mln::core::ForbidReentry scope(callback_reentry);
    registration_->callbacks.render(registration_->callbacks.user_data,
                                    &parameters);
  }

  void contextLost() override {
    context_ = nullptr;
    CallbackContextScope contextScope(nullptr);
    mln::core::ForbidReentry scope(callback_reentry);
    registration_->callbacks.context_lost(registration_->callbacks.user_data);
    graphics_ = {};
  }

  void deinitialize() override {
    CallbackContextScope contextScope(context_);
    mln::core::ForbidReentry scope(callback_reentry);
    registration_->callbacks.deinitialize(registration_->callbacks.user_data);
    graphics_ = {};
  }

private:
  std::shared_ptr<Registration> registration_;
  mln_custom_graphics_context graphics_{};
    const mln::gfx::Context *context_ = nullptr;
};

bool valid_id(mln_buffer_view id) {
  return id.data != nullptr && id.size != 0 &&
         std::memchr(id.data, '\0', id.size) == nullptr;
}

bool valid_callbacks(const mln_custom_layer_callbacks *callbacks) {
  if (callbacks == nullptr || callbacks->size != sizeof(*callbacks) ||
      callbacks->abi_version != MLN_CUSTOM_LAYER_ABI_VERSION)
    return false;
  return callbacks->initialize && callbacks->pre_render && callbacks->render &&
         callbacks->context_lost && callbacks->deinitialize &&
         callbacks->release_user_data;
}

bool valid_completion(const mln_completion *completion) {
  return completion != nullptr && completion->size == sizeof(*completion) &&
         completion->callback && completion->release_user_data &&
         completion->queue == 0;
}

mln_status insert(mln::core::MapObject &live, const std::string &id,
                  const std::optional<std::string> &before,
                  const std::shared_ptr<Registration> &registration) {
  auto &style = mln::core::map_native(live).getStyle();
  if (style.getLayer(id) != nullptr) {
    mln::core::set_thread_error("custom layer id already exists");
    return MLN_STATUS_INVALID_ARGUMENT;
  }
  if (before && style.getLayer(*before) == nullptr) {
    mln::core::set_thread_error("custom layer before id does not exist");
    return MLN_STATUS_NOT_FOUND;
  }
  style.addLayer(std::make_unique<mln::style::CustomLayer>(
                     id, std::make_unique<Host>(registration)),
                 before);
  return MLN_STATUS_OK;
}
} // namespace

// Internal extension entry point: never expose or retain this pointer in Rust.
// Only compositor retirement in the current host callback may use it.
extern "C" void *mln_custom_layer_vulkan_context() noexcept {
#if MLN_RENDER_BACKEND_VULKAN
  return const_cast<mln::vulkan::Context *>(
      static_cast<const mln::vulkan::Context *>(callback_context));
#else
  return nullptr;
#endif
}

mln_status mln_custom_layer_add(mln_map map, mln_buffer_view id,
                                const mln_custom_layer_callbacks *callbacks,
                                mln_buffer_view before,
                                const mln_completion *completion,
                                mln_diagnostic *out_diagnostic) noexcept {
  return mln::c_api::status_boundary(out_diagnostic, [&]() -> mln_status {
    if (!valid_id(id) ||
        (before.data != nullptr ? !valid_id(before) : before.size != 0)) {
      mln::core::set_thread_error(
          "custom layer ids must be nonempty without embedded NUL bytes");
      return MLN_STATUS_INVALID_ARGUMENT;
    }
    if (!valid_callbacks(callbacks)) {
      mln::core::set_thread_error(
          "custom layer callback ABI mismatch or missing callback");
      return MLN_STATUS_INVALID_ARGUMENT;
    }
    if (!valid_completion(completion)) {
      mln::core::set_thread_error(
          "custom layer requires a direct completion callback and release");
      return MLN_STATUS_INVALID_ARGUMENT;
    }
    auto owned_id = std::string(static_cast<const char *>(id.data), id.size);
    auto owned_before = std::optional<std::string>{};
    if (before.data != nullptr)
      owned_before.emplace(static_cast<const char *>(before.data), before.size);
    auto registration = std::make_shared<Registration>(*callbacks);
    const auto status = mln::core::submit_map_command(
        map,
        [owned_id = std::move(owned_id), owned_before = std::move(owned_before),
         registration](mln::core::MapObject &live) -> mln_status {
          return insert(live, owned_id, owned_before, registration);
        },
        completion);
    if (status == MLN_STATUS_OK)
      registration->accepted.store(true, std::memory_order_release);
    return status;
  });
}
