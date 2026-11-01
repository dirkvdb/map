#pragma once
#include <cstdint>
#include <mln/util/mat4.hpp>

namespace mln {
struct TestSize {
  uint32_t width = 801;
  uint32_t height = 603;
};
struct TestLatLng {
  double latitude() const { return 51.0; }
  double longitude() const { return 4.0; }
};
struct TransformState {
  mat4 standard{};
  TestSize getSize() const { return {}; }
  TestLatLng getLatLng() const { return {}; }
  double getZoom() const { return 7.5; }
  double getBearing() const { return 0.25; }
  double getPitch() const { return 0.5; }
  double getFieldOfView() const { return 0.75; }
  void getProjMatrix(mat4 &output) const { output = standard; }
};
struct TestTransformParameters {
  mat4 alignedProjMatrix{};
  mat4 nearClippedProjMatrix{};
};
class PaintParameters {
public:
  TransformState state;
  TestTransformParameters transformParams;
  uint32_t currentLayer = 0;
  uint32_t layerGroupCount = 1;
  bool layerGroupUpload = false;
  uint32_t opaquePassCutoff = 0;
  float pixelRatio = 2.5f;
  static constexpr int numSublayers = 3;
  static constexpr float depthEpsilon = 1.0f / (1 << 11);
};
} // namespace mln
