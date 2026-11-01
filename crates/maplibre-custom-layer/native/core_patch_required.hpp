#pragma once

#include <mln/style/layers/custom_layer_render_parameters.hpp>
#include <type_traits>

#ifndef MLN_CUSTOM_LAYER_RASTER_PARAMETERS_VERSION
#error                                                                         \
    "maplibre-custom-layer requires patches/0001-custom-layer-raster-parameters.patch applied to the native core"
#elif MLN_CUSTOM_LAYER_RASTER_PARAMETERS_VERSION != 1
#error "Unsupported native custom-layer raster parameter patch version"
#else
static_assert(std::is_same_v<decltype(mln::style::CustomLayerRenderParameters::
                                          rasterProjectionMatrix),
                             std::array<double, 16>>);
static_assert(
    std::is_same_v<
        decltype(mln::style::CustomLayerRenderParameters::flatDepthOffset),
        double>);
static_assert(
    std::is_same_v<
        decltype(mln::style::CustomLayerRenderParameters::pixelRatio), double>);
static_assert(
    std::is_same_v<decltype(mln::style::CustomLayerRenderParameters::depthTest),
                   bool>);
#endif
