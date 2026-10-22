#pragma once

#if defined(MLN_WEBGPU_IMPL_FFI)
#include <mln/style/layer.hpp>
#include "rust/cxx.h"

namespace mln::bridge {
struct WgpuLayerHost;
std::unique_ptr<mln::style::Layer> new_wgpu_custom_layer(rust::Str id, rust::Box<WgpuLayerHost> host);
}
#endif
