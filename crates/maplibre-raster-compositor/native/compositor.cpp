#include "backend.hpp"
#include <algorithm>
#include <cstring>
#include <new>
#include <string>

struct map_compositor {
  std::unique_ptr<map_raster::Backend> backend;
  uint32_t backend_id = MLN_CUSTOM_BACKEND_OTHER;
};

namespace {
template <class Function>
int boundary(char *error, size_t size, Function function) noexcept {
  try {
    function();
    return 0;
  } catch (const std::exception &exception) {
    if (error && size) {
      const auto length = std::min(std::strlen(exception.what()), size - 1);
      std::memcpy(error, exception.what(), length);
      error[length] = '\0';
    }
    return 1;
  } catch (...) {
    if (error && size) {
      const char *message = "unknown native compositor exception";
      const auto length = std::min(std::strlen(message), size - 1);
      std::memcpy(error, message, length);
      error[length] = '\0';
    }
    return 1;
  }
}
} // namespace

extern "C" {
map_compositor *map_compositor_new() {
  return new (std::nothrow) map_compositor;
}
int map_compositor_prepare(map_compositor *self,
                           const mln_custom_frame_parameters *parameters,
                           const map_surface *surface, char *error,
                           size_t size) {
  return boundary(error, size, [&] {
    if (!self || !parameters || !surface)
      throw std::invalid_argument("null compositor input");
    if (self->backend && self->backend_id != parameters->graphics.backend)
      throw std::runtime_error(
          "backend changed without context_lost/deinitialize");
    if (!self->backend) {
      self->backend = map_raster::make_backend(parameters->graphics);
      self->backend_id = parameters->graphics.backend;
    }
    self->backend->prepare(*parameters, *surface);
  });
}
int map_compositor_render(map_compositor *self,
                          const mln_custom_frame_parameters *parameters,
                          char *error, size_t size) {
  return boundary(error, size, [&] {
    if (!self || !parameters || !self->backend)
      throw std::runtime_error("render before prepare");
    if (self->backend_id != parameters->graphics.backend)
      throw std::runtime_error("backend mismatch");
    self->backend->render(*parameters);
  });
}
int map_compositor_deinitialize(map_compositor *self, char *error,
                                size_t size) {
  return boundary(error, size, [&] {
    if (self && self->backend) {
      self->backend->deinitialize();
      self->backend.reset();
    }
  });
}
void map_compositor_context_lost(map_compositor *self) {
  if (self && self->backend) {
    self->backend->abandon();
    self->backend.reset();
  }
}
void map_compositor_delete(map_compositor *self) {
  map_compositor_context_lost(self);
  delete self;
}
}
