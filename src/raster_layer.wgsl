struct Draw {
    matrix: mat4x4<f32>,
    uv: vec4<f32>,
    range: vec2<f32>,
    opacity: f32,
    padding: f32,
};

@group(0) @binding(0) var<uniform> draw: Draw;
@group(0) @binding(1) var values: texture_2d<f32>;
@group(0) @binding(2) var valid: texture_2d<f32>;
@group(0) @binding(3) var palette: texture_2d<f32>;

struct Vertex {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vertex(@builtin(vertex_index) ix: u32) -> Vertex {
    let corners = array<vec2<f32>, 6>(
        vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
        vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0),
    );
    var out: Vertex;
    out.position = draw.matrix * vec4(corners[ix], 0.0, 1.0);
    out.uv = draw.uv.xy + corners[ix] * draw.uv.zw;
    return out;
}

@fragment
fn fragment(in: Vertex) -> @location(0) vec4<f32> {
    let size = vec2<i32>(textureDimensions(values));
    let pixel = clamp(vec2<i32>(in.uv * vec2<f32>(size)), vec2(0), size - vec2(1));
    if textureLoad(valid, pixel, 0).r < 0.5 {
        discard;
    }
    let value = textureLoad(values, pixel, 0).r;
    let bounded = clamp(value, draw.range.x, draw.range.y);
    let t = (bounded - draw.range.x) / (draw.range.y - draw.range.x);
    // Match the legend's nearest palette-entry rounding, including endpoints.
    let color = textureLoad(palette, vec2<i32>(i32(floor(t * 255.0 + 0.5)), 0), 0);
    let alpha = color.a * draw.opacity;
    return vec4(color.rgb * alpha, alpha);
}
