#pragma once
#include <array>

// Constructor-only test doubles; no official binding implementation is copied.
namespace mln {
using mat4 = std::array<double, 16>;
namespace util {
inline double rad2deg(double value) { return value * 57.29577951308232; }
} // namespace util
} // namespace mln
