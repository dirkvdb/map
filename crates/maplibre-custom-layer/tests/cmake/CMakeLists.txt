cmake_minimum_required(VERSION 3.28)
project(custom_layer_injection_smoke LANGUAGES CXX)

# No native build: verify the project hook waits for this target and injects
# the real source exactly once, without requiring upstream sources or patches.
add_library(maplibre_native_c_objects OBJECT)

function(verify_injection)
  get_target_property(sources maplibre_native_c_objects SOURCES)
  list(LENGTH sources source_count)
  if(NOT source_count EQUAL 1)
    message(FATAL_ERROR "Expected exactly one injected source, got: ${sources}")
  endif()
  get_filename_component(actual "${sources}" REALPATH)
  get_filename_component(expected
    "${CMAKE_CURRENT_FUNCTION_LIST_DIR}/../../native/custom_layer.cpp" REALPATH)
  if(NOT actual STREQUAL expected)
    message(FATAL_ERROR "Wrong injected source: ${actual}")
  endif()
endfunction()

cmake_language(DEFER CALL verify_injection)

# Optional test-only target compiling the exact guard used by the real adapter.
# The native regression tests provide patched and unpatched core header roots.
if(MLN_CUSTOM_TEST_CORE_INCLUDE)
  add_library(core_patch_contract OBJECT ../native/core_patch_contract.cpp)
  target_include_directories(core_patch_contract PRIVATE
    ../../native "${MLN_CUSTOM_TEST_CORE_INCLUDE}")
  target_compile_features(core_patch_contract PRIVATE cxx_std_20)
endif()
