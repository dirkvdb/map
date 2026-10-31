@group(0) @binding(0) var color_texture: texture_2d<f32>;
@group(0) @binding(1) var depth_texture: texture_depth_2d;
@group(0) @binding(2) var<uniform> opacity: vec4<f32>;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) screen_uv: vec2<f32>,
};

@vertex
fn vertex(@builtin(vertex_index) index: u32) -> VertexOutput {
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    var output: VertexOutput;
    output.position = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    output.screen_uv = uv;
    return output;
}

struct FragmentOutput {
    @location(0) color: vec4<f32>,
    @builtin(frag_depth) depth: f32,
};

@fragment
fn fragment_color(input: VertexOutput) -> @location(0) vec4<f32> {
    return surface_color(input);
}

fn surface_color(input: VertexOutput) -> vec4<f32> {
    let dimensions = textureDimensions(color_texture);
    let pixel = clamp(vec2<i32>(input.screen_uv * vec2<f32>(dimensions)), vec2<i32>(0), vec2<i32>(dimensions) - vec2<i32>(1));
    let color = textureLoad(color_texture, pixel, 0) * opacity.x;
    if color.a == 0.0 { discard; }
    return color;
}

@fragment
fn fragment(input: VertexOutput) -> FragmentOutput {
    let dimensions = textureDimensions(color_texture);
    let pixel = clamp(vec2<i32>(input.screen_uv * vec2<f32>(dimensions)), vec2<i32>(0), vec2<i32>(dimensions) - vec2<i32>(1));
    var output: FragmentOutput;
    output.color = surface_color(input);
    output.depth = textureLoad(depth_texture, pixel, 0);
    return output;
}
