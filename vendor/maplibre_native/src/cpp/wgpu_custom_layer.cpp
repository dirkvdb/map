#include "wgpu_custom_layer.h"

#if defined(MLN_WEBGPU_IMPL_FFI)
#include "maplibre_native/src/bridge.rs.h"
#include <mln/style/layers/custom_layer.hpp>
#include <mln/style/layers/custom_layer_host.hpp>
#include <mln/style/layers/webgpu/custom_layer_render_parameters.hpp>

namespace mln::bridge {
namespace {
class RustWgpuLayer final : public mln::style::CustomLayerHost {
public:
    explicit RustWgpuLayer(rust::Box<WgpuLayerHost> host_) : host(std::move(host_)) {}
    void initialize(const mln::style::CustomLayerInitParameters&) override {}
    void render(const mln::style::CustomLayerRenderParameters& base) override {
        const auto& params = static_cast<const mln::style::webgpu::CustomLayerRenderParameters&>(base);
        WgpuLayerParameters context;
        context.width = params.width;
        context.height = params.height;
        context.latitude = params.latitude;
        context.longitude = params.longitude;
        context.zoom = params.zoom;
        context.projection = params.projectionMatrix;
        context.color_format = params.colorFormat;
        context.depth_format = params.depthStencilFormat;
        context.depth_test = params.depthTest;
        wgpu_layer_render(*host, params.encoder, context);
    }
    void contextLost() override { wgpu_layer_deinitialize(*host); }
    void deinitialize() override { wgpu_layer_deinitialize(*host); }
private:
    rust::Box<WgpuLayerHost> host;
};
}

std::unique_ptr<mln::style::Layer> new_wgpu_custom_layer(rust::Str id, rust::Box<WgpuLayerHost> host) {
    return std::make_unique<mln::style::CustomLayer>(std::string(id), std::make_unique<RustWgpuLayer>(std::move(host)));
}
}
#endif
