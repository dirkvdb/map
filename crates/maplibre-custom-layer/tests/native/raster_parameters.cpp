#include "core_patch_required.hpp"
#include <cassert>
#include <cmath>
#include <iostream>
#include <mln/renderer/paint_parameters.hpp>

using Parameters = mln::style::CustomLayerRenderParameters;

mln::PaintParameters paint(uint32_t count, uint32_t uploadIndex,
                           uint32_t cutoff, bool upload) {
  mln::PaintParameters result;
  result.layerGroupCount = count;
  result.currentLayer = upload ? uploadIndex : count - 1 - uploadIndex;
  result.opaquePassCutoff = cutoff;
  result.layerGroupUpload = upload;
  for (size_t i = 0; i < 16; ++i) {
    result.state.standard[i] = 1.0 + i * 0.125;
    result.transformParams.nearClippedProjMatrix[i] = 2.0 + i * 0.125;
    result.transformParams.alignedProjMatrix[i] = 3.0 + i * 0.125;
  }
  return result;
}

void checkLayer(uint32_t count, uint32_t uploadIndex, uint32_t cutoff) {
  const auto beforePaint = paint(count, uploadIndex, cutoff, true);
  const auto drawPaint = paint(count, uploadIndex, cutoff, false);
  const Parameters before(beforePaint);
  const Parameters draw(drawPaint);
  const bool enabled = count - 1 - uploadIndex >= cutoff;
  // Exact binary fractions, matching sublayer-zero native float arithmetic.
  const double expectedOffset =
      enabled ? (1 + uploadIndex) * 3.0 / 2048.0 : 0.0;
  auto expected = beforePaint.transformParams.alignedProjMatrix;
  expected[14] -= expectedOffset;
  assert(before.rasterProjectionMatrix == expected);
  assert(draw.rasterProjectionMatrix == before.rasterProjectionMatrix);
  assert(before.flatDepthOffset == expectedOffset);
  assert(draw.flatDepthOffset == expectedOffset);
  assert(std::isfinite(draw.flatDepthOffset));
  assert(before.depthTest == enabled);
  assert(draw.depthTest == enabled);
  assert(draw.pixelRatio == 2.5);
  assert(draw.projectionMatrix == beforePaint.state.standard);
  assert(draw.nearClippedProjectionMatrix ==
         beforePaint.transformParams.nearClippedProjMatrix);
}

std::array<double, 4> project(const mln::mat4 &matrix,
                              const std::array<double, 4> &point) {
  std::array<double, 4> result{};
  for (size_t row = 0; row < 4; ++row) {
    for (size_t column = 0; column < 4; ++column) {
      result[row] += matrix[4 * column + row] * point[column];
    }
  }
  return result;
}

void checkRegistrationAndDepthOrder() {
  const auto state = paint(5, 0, 0, false);
  const auto aligned = state.transformParams.alignedProjMatrix;
  // A point after the app's affine tile/model transform, with non-unit clip W.
  const std::array<double, 4> worldPoint{200.0, 300.0, 0.0, 1.0};
  const auto reference = project(aligned, worldPoint);
  double previousDepth = reference[2] / reference[3];
  for (uint32_t uploadIndex = 0; uploadIndex < 5; ++uploadIndex) {
    const Parameters parameters(paint(5, uploadIndex, 0, false));
    const auto clip = project(parameters.rasterProjectionMatrix, worldPoint);
    assert(clip[0] == reference[0]);
    assert(clip[1] == reference[1]);
    assert(clip[3] == reference[3]);
    assert(std::abs((reference[2] - clip[2]) - parameters.flatDepthOffset) <
           1e-10);
    const double depth = clip[2] / clip[3];
    assert(depth <
           previousDepth); // Higher native layers are closer for LessEqual.
    previousDepth = depth;
  }
}

int main() {
  for (const uint32_t count : {1u, 2u, 5u, 9u}) {
    for (uint32_t cutoff = 0; cutoff <= count; ++cutoff) {
      for (uint32_t uploadIndex = 0; uploadIndex < count; ++uploadIndex) {
        checkLayer(count, uploadIndex, cutoff);
      }
    }
  }
  checkRegistrationAndDepthOrder();
  std::cout << "raster constructor: all layer positions, cutoffs, phases, "
               "projection and depth checks passed\n";
}
