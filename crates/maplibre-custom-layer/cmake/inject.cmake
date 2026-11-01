# Pass this absolute path as CMAKE_PROJECT_INCLUDE when configuring the pinned
# official FFI. project() runs this before its targets exist; DEFER injects the
# adapter only once the FFI directory has finished defining its object target.
include_guard(GLOBAL)

function(mln_custom_layer_inject)
  if(NOT TARGET maplibre_native_c_objects)
    message(FATAL_ERROR "Custom layer injection requires maplibre_native_c_objects")
  endif()
  set(bridge_root "${CMAKE_CURRENT_FUNCTION_LIST_DIR}/..")
  # custom_layer.cpp includes core_patch_required.hpp: compiling this target
  # fails explicitly if the required optional core patch has not been applied.
  target_sources(maplibre_native_c_objects PRIVATE
    "${bridge_root}/native/custom_layer.cpp")
  target_include_directories(maplibre_native_c_objects PRIVATE
    "${bridge_root}/native/include")
endfunction()

cmake_language(DEFER CALL mln_custom_layer_inject)
