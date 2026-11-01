include_guard(GLOBAL)

function(map_raster_compositor_inject)
  if(NOT TARGET maplibre_native_c_objects)
    message(FATAL_ERROR "Raster compositor injection requires maplibre_native_c_objects")
  endif()
  set(compositor_root "${CMAKE_CURRENT_FUNCTION_LIST_DIR}/..")
  target_sources(maplibre_native_c_objects PRIVATE "${compositor_root}/native/compositor.cpp")
  target_include_directories(maplibre_native_c_objects PRIVATE
    "${compositor_root}/native"
    "${compositor_root}/../maplibre-custom-layer/native/include")
  if(MLN_FFI_RENDER_BACKEND STREQUAL "vulkan")
    target_sources(maplibre_native_c_objects PRIVATE
      "${compositor_root}/native/vulkan.cpp"
      "${compositor_root}/native/vulkan_retirement.cpp")
  elseif(MLN_FFI_RENDER_BACKEND STREQUAL "metal")
    target_sources(maplibre_native_c_objects PRIVATE "${compositor_root}/native/metal.mm")
    set_source_files_properties("${compositor_root}/native/metal.mm" PROPERTIES COMPILE_OPTIONS "-fobjc-arc")
  else()
    message(FATAL_ERROR "Raster compositor requires a Vulkan or Metal official FFI backend")
  endif()
endfunction()

cmake_language(DEFER CALL map_raster_compositor_inject)
