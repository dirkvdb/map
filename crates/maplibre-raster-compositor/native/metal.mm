#include "backend.hpp"
#import <Metal/Metal.h>
#include <cstring>
#include <string>
#include <utility>

namespace map_raster {
namespace {
constexpr const char *shader = R"metal(
#include <metal_stdlib>
using namespace metal;
struct Vertex { float4 position [[position]]; float2 uv; };
struct Pixel { float4 color [[color(0)]]; float depth [[depth(any)]]; };
struct Size { uint width; uint height; };
vertex Vertex fullscreen(uint index [[vertex_id]]) {
    float2 p = float2((index << 1) & 2, index & 2);
    return { float4(p.x * 2.0 - 1.0, 1.0 - p.y * 2.0, 0.0, 1.0), p };
}
fragment Pixel composite(Vertex in [[stage_in]],
    const device uchar4 *color [[buffer(0)]],
    const device float *depth [[buffer(1)]], constant Size &size [[buffer(2)]]) {
    uint2 pixel = min(uint2(in.uv * float2(size.width, size.height)), uint2(size.width-1, size.height-1));
    uint index = pixel.y * size.width + pixel.x;
    float4 value = float4(color[index]) / 255.0;
    if (value.a == 0.0) discard_fragment();
    // Pinned native Metal shaders use raw GL z/w, not GL window-depth mapping.
    float nativeDepth = depth[index] * 2.0 - 1.0;
    if (nativeDepth < 0.0 || nativeDepth > 1.0) discard_fragment();
    return { value, nativeDepth };
}
)metal";

std::string diagnostic(NSError *error, const char *fallback) {
  const char *message = error.localizedDescription.UTF8String;
  return message ? message : fallback;
}
MTLRenderPassDescriptor *
pass_descriptor(const mln_custom_graphics_context &graphics) {
  auto *pass =
      (__bridge MTLRenderPassDescriptor *)graphics.metal_render_pass_descriptor;
  if (!pass || !pass.colorAttachments[0].texture)
    throw std::runtime_error(
        "Metal callback lacks native attachment descriptor");
  return pass;
}
id<MTLDevice> device_for(const mln_custom_graphics_context &graphics) {
  id<MTLDevice> device = (__bridge id<MTLDevice>)graphics.metal_device;
  if (!device)
    device = pass_descriptor(graphics).colorAttachments[0].texture.device;
  if (!device)
    throw std::runtime_error("Metal callback lacks device");
  return device;
}
struct Frame {
  id<MTLBuffer> color, depth;
  id<MTLRenderPipelineState> pipeline;
  id<MTLDepthStencilState> depth_state;
  uint32_t width, height;
  bool depth_test, extruded;
  MTLPixelFormat color_format, depth_format, stencil_format;
  NSUInteger samples;
  Frame(const map_surface &surface, bool test)
      : width(surface.width), height(surface.height), depth_test(test),
        extruded(surface.extruded) {}
  bool matches_pipeline(const Frame &other) const {
    return color_format == other.color_format &&
           depth_format == other.depth_format &&
           stencil_format == other.stencil_format && samples == other.samples &&
           extruded == other.extruded && depth_test == other.depth_test;
  }
  void upload(id<MTLDevice> device, const map_surface &surface) {
    const uint64_t bytes = uint64_t(width) * height * 4;
    if (!width || !height || !surface.color || !surface.depth ||
        bytes > device.maxBufferLength)
      throw std::runtime_error(
          "invalid surface or Metal maxBufferLength exceeded");
    color = [device newBufferWithBytes:surface.color
                                length:bytes
                               options:MTLResourceStorageModeShared];
    depth = [device newBufferWithBytes:surface.depth
                                length:bytes
                               options:MTLResourceStorageModeShared];
    if (!color || !depth)
      throw std::runtime_error("Metal surface buffer allocation failed");
  }
  void create_pipeline(id<MTLDevice> device, id<MTLLibrary> library,
                        MTLRenderPassDescriptor *pass, const Frame *previous) {
    color_format = pass.colorAttachments[0].texture.pixelFormat;
    depth_format = pass.depthAttachment.texture.pixelFormat;
    stencil_format = pass.stencilAttachment.texture.pixelFormat;
    samples = pass.colorAttachments[0].texture.sampleCount;
    if ((extruded || depth_test) && !pass.depthAttachment.texture)
      throw std::runtime_error(
          "Metal depth testing requested without depth attachment");
    if (previous && matches_pipeline(*previous)) {
      pipeline = previous->pipeline;
      depth_state = previous->depth_state;
      return;
    }
    auto *descriptor = [MTLRenderPipelineDescriptor new];
    descriptor.vertexFunction = [library newFunctionWithName:@"fullscreen"];
    descriptor.fragmentFunction = [library newFunctionWithName:@"composite"];
    descriptor.rasterSampleCount = samples;
    descriptor.depthAttachmentPixelFormat = depth_format;
    descriptor.stencilAttachmentPixelFormat = stencil_format;
    auto *attachment = descriptor.colorAttachments[0];
    attachment.pixelFormat = color_format;
    attachment.blendingEnabled = YES;
    attachment.sourceRGBBlendFactor = attachment.sourceAlphaBlendFactor =
        MTLBlendFactorOne;
    attachment.destinationRGBBlendFactor =
        attachment.destinationAlphaBlendFactor =
            MTLBlendFactorOneMinusSourceAlpha;
    attachment.rgbBlendOperation = attachment.alphaBlendOperation =
        MTLBlendOperationAdd;
    NSError *error = nil;
    pipeline = [device newRenderPipelineStateWithDescriptor:descriptor
                                                      error:&error];
    if (!pipeline)
      throw std::runtime_error(
          diagnostic(error, "Metal pipeline compilation failed"));
    auto *depthDescriptor = [MTLDepthStencilDescriptor new];
    depthDescriptor.depthCompareFunction = extruded || depth_test
                                               ? MTLCompareFunctionLessEqual
                                               : MTLCompareFunctionAlways;
    depthDescriptor.depthWriteEnabled = extruded;
    depth_state = [device newDepthStencilStateWithDescriptor:depthDescriptor];
    if (!depth_state)
      throw std::runtime_error("Metal depth state creation failed");
  }
};
class MetalBackend final : public Backend {
  id<MTLDevice> device;
  id<MTLLibrary> library;
  std::shared_ptr<Frame> frame;

public:
  explicit MetalBackend(const mln_custom_graphics_context &graphics)
      : device(device_for(graphics)) {
    NSError *error = nil;
    library =
        [device newLibraryWithSource:[NSString stringWithUTF8String:shader]
                             options:nil
                               error:&error];
    if (!library)
      throw std::runtime_error(
          diagnostic(error, "Metal shader compilation failed"));
  }
  void prepare(const mln_custom_frame_parameters &parameters,
               const map_surface &surface) override {
    @autoreleasepool {
      if (device != device_for(parameters.graphics))
        throw std::runtime_error("Metal device changed without cleanup");
      auto next = std::make_shared<Frame>(surface, parameters.depth_test);
      next->upload(device, surface);
      next->create_pipeline(device, library,
                             pass_descriptor(parameters.graphics), frame.get());
      frame = std::move(next);
    }
  }
  void render(const mln_custom_frame_parameters &parameters) override {
    @autoreleasepool {
      const auto &graphics = parameters.graphics;
      auto encoder =
          (__bridge id<MTLRenderCommandEncoder>)graphics.metal_encoder;
      auto command =
          (__bridge id<MTLCommandBuffer>)graphics.metal_command_buffer;
      if (!frame || !encoder || !command || device != device_for(graphics))
        throw std::runtime_error(
            "missing prepared surface or active Metal encoder/command buffer");
      auto *pass = pass_descriptor(graphics);
      if (frame->depth_test != parameters.depth_test ||
          frame->color_format != pass.colorAttachments[0].texture.pixelFormat ||
          frame->depth_format != pass.depthAttachment.texture.pixelFormat ||
          frame->stencil_format != pass.stencilAttachment.texture.pixelFormat ||
          frame->samples != pass.colorAttachments[0].texture.sampleCount)
        throw std::runtime_error(
            "Metal attachment/depth mode changed after prepare");
      // Keep all resources until GPU completion, even with unretained command
      // buffers. Install before encoding: block allocation failure cannot
      // strand recorded work.
      const auto retained = frame;
      [command addCompletedHandler:^(id<MTLCommandBuffer>) {
        (void)retained;
      }];
      const MTLViewport viewport{
          0,
          0,
          double(pass.colorAttachments[0].texture.width),
          double(pass.colorAttachments[0].texture.height),
          0,
          1};
      const MTLScissorRect scissor{0, 0, pass.colorAttachments[0].texture.width,
                                   pass.colorAttachments[0].texture.height};
      [encoder setViewport:viewport];
      [encoder setScissorRect:scissor];
      // Match native RenderPass::resetState cache defaults; native dirty-state
      // handling resets caches but does not reset these encoder values.
      [encoder setCullMode:MTLCullModeNone];
      [encoder setFrontFacingWinding:MTLWindingClockwise];
      [encoder setStencilReferenceValue:0];
      [encoder setRenderPipelineState:frame->pipeline];
      [encoder setDepthStencilState:frame->depth_state];
      [encoder setFragmentBuffer:frame->color offset:0 atIndex:0];
      [encoder setFragmentBuffer:frame->depth offset:0 atIndex:1];
      const uint32_t size[] = {frame->width, frame->height};
      [encoder setFragmentBytes:size length:sizeof(size) atIndex:2];
      [encoder drawPrimitives:MTLPrimitiveTypeTriangle
                  vertexStart:0
                  vertexCount:3];
    }
  }
  void deinitialize() override {
    frame.reset();
    library = nil;
    device = nil;
  }
  void abandon() noexcept override {
    frame.reset();
    library = nil;
    device = nil;
  }
};
} // namespace
std::unique_ptr<Backend>
make_backend(const mln_custom_graphics_context &graphics) {
  if (graphics.backend != MLN_CUSTOM_BACKEND_METAL)
    throw std::runtime_error("this native archive supports Metal only");
  return std::make_unique<MetalBackend>(graphics);
}
} // namespace map_raster
