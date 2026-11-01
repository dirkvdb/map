#pragma once
#include "compositor.h"
#include <memory>
#include <stdexcept>

namespace map_raster {
class Backend {
public:
  virtual ~Backend() = default;
  virtual void prepare(const mln_custom_frame_parameters &,
                       const map_surface &) = 0;
  virtual void render(const mln_custom_frame_parameters &) = 0;
  virtual void deinitialize() = 0;
  virtual void abandon() noexcept = 0;
};
std::unique_ptr<Backend> make_backend(const mln_custom_graphics_context &);
} // namespace map_raster
