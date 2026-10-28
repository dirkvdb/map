struct Draw {
    matrix: mat4x4<f32>,
    uv: vec4<f32>,
    previous_uv: vec4<f32>,
    range: vec2<f32>,
    previous_range: vec2<f32>,
    scale_offset: vec2<f32>,
    previous_scale_offset: vec2<f32>,
    opacity: f32,
    blend: f32,
    tile_size: f32,
    padding: f32,
    tile_id: vec4<u32>,
    clamp_colors: vec4<u32>,
};

@group(0) @binding(0) var<uniform> draw: Draw;
@group(0) @binding(1) var values: texture_2d<f32>;
@group(0) @binding(2) var valid: texture_2d<f32>;
@group(0) @binding(3) var palette: texture_2d<f32>;
@group(0) @binding(4) var previous_values: texture_2d<f32>;
@group(0) @binding(5) var previous_valid: texture_2d<f32>;
@group(0) @binding(6) var previous_palette: texture_2d<f32>;

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
    out.uv = corners[ix];
    return out;
}

fn color_at(values: texture_2d<f32>, valid: texture_2d<f32>, palette: texture_2d<f32>, uv: vec2<f32>, range: vec2<f32>, scale_offset: vec2<f32>, clamp_colors: u32) -> vec4<f32> {
    let size = vec2<i32>(textureDimensions(values));
    let pixel = clamp(vec2<i32>(uv * vec2<f32>(size)), vec2(0), size - vec2(1));
    if textureLoad(valid, pixel, 0).r < 0.5 {
        return vec4(0.0);
    }
    let value = textureLoad(values, pixel, 0).r * scale_offset.x + scale_offset.y;
    if clamp_colors == 0u && (value < range.x || value > range.y) {
        return vec4(0.0);
    }
    let bounded = clamp(value, range.x, range.y);
    let t = (bounded - range.x) / (range.y - range.x);
    // Match the legend's nearest palette-entry rounding, including endpoints.
    let color = textureLoad(palette, vec2<i32>(i32(floor(t * 255.0 + 0.5)), 0), 0);
    return vec4(color.rgb * color.a, color.a);
}

@fragment
fn fragment(in: Vertex) -> @location(0) vec4<f32> {
    if draw.blend <= 0.0 {
        return color_at(previous_values, previous_valid, previous_palette,
            draw.previous_uv.xy + in.uv * draw.previous_uv.zw, draw.previous_range, draw.previous_scale_offset, draw.clamp_colors.y) * draw.opacity;
    }
    let current = color_at(values, valid, palette, draw.uv.xy + in.uv * draw.uv.zw, draw.range, draw.scale_offset, draw.clamp_colors.x);
    if draw.blend >= 1.0 {
        return current * draw.opacity;
    }
    let previous = color_at(previous_values, previous_valid, previous_palette,
        draw.previous_uv.xy + in.uv * draw.previous_uv.zw, draw.previous_range, draw.previous_scale_offset, draw.clamp_colors.y);
    // Blend premultiplied colors, not raw values. Independent nodata masks and
    // palettes must not leak a basemap flash between two opaque band pixels.
    return mix(previous, current, draw.blend) * draw.opacity;
}
