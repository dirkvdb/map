#version 450
layout(set = 0, binding = 0, std430) readonly buffer Color { uint rgba[]; } color;
layout(set = 0, binding = 1, std430) readonly buffer Depth { float z[]; } depth;
layout(push_constant) uniform Parameters { uint width; uint height; float cosine; float sine; } pc;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 outputColor;
void main() {
    // Inverse native clockwise clip-space pre-rotation; top-left raster origin.
    vec2 p = uv * 2.0 - 1.0;
    vec2 source = vec2(pc.cosine * p.x + pc.sine * p.y,
                       -pc.sine * p.x + pc.cosine * p.y) * 0.5 + 0.5;
    if (any(lessThan(source, vec2(0.0))) || any(greaterThanEqual(source, vec2(1.0)))) discard;
    uvec2 pixel = min(uvec2(source * vec2(pc.width, pc.height)), uvec2(pc.width - 1, pc.height - 1));
    uint index = pixel.y * pc.width + pixel.x;
    outputColor = unpackUnorm4x8(color.rgba[index]);
    if (outputColor.a == 0.0) discard;
    // Pinned native shaders use raw GL clip z/w as Vulkan window depth.
    // Undo the parent's GL-to-wgpu z'=0.5*z+0.5*w conversion.
    float nativeDepth = depth.z[index] * 2.0 - 1.0;
    if (nativeDepth < 0.0 || nativeDepth > 1.0) discard;
    gl_FragDepth = nativeDepth;
}
