cmake_minimum_required(VERSION 3.28)
project(map_raster_compositor_smoke LANGUAGES CXX)
enable_testing()
set(MAP_COMPOSITOR_FFI_SOURCE "" CACHE PATH "Prepared official FFI source checkout")
if(NOT EXISTS "${MAP_COMPOSITOR_FFI_SOURCE}/include/maplibre_native_c.h")
  message(FATAL_ERROR "Set MAP_COMPOSITOR_FFI_SOURCE to the prepared official FFI checkout")
endif()
find_package(Vulkan REQUIRED)
add_executable(vulkan_smoke
  vulkan_smoke.cpp ../native/compositor.cpp ../native/vulkan.cpp)
target_compile_features(vulkan_smoke PRIVATE cxx_std_20)
target_include_directories(vulkan_smoke PRIVATE
  ../native ../../maplibre-custom-layer/native/include
  "${MAP_COMPOSITOR_FFI_SOURCE}/include")
target_link_libraries(vulkan_smoke PRIVATE Vulkan::Vulkan)
add_test(NAME vulkan_color_depth_retirement COMMAND vulkan_smoke)
