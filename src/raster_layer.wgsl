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
    previous_extrusion_height: f32,
    tile_id: vec4<u32>,
    clamp_colors: vec4<u32>,
    extrusion: vec4<f32>,
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

struct RasterSample {
    color: vec4<f32>,
    height: f32,
};

fn sample_at(values: texture_2d<f32>, valid: texture_2d<f32>, palette: texture_2d<f32>, uv: vec2<f32>, range: vec2<f32>, scale_offset: vec2<f32>, clamp_colors: u32) -> RasterSample {
    let size = vec2<i32>(textureDimensions(values));
    let pixel = clamp(vec2<i32>(uv * vec2<f32>(size)), vec2(0), size - vec2(1));
    if textureLoad(valid, pixel, 0).r < 0.5 {
        return RasterSample(vec4(0.0), 0.0);
    }
    let value = textureLoad(values, pixel, 0).r * scale_offset.x + scale_offset.y;
    if clamp_colors == 0u && (value < range.x || value > range.y) {
        return RasterSample(vec4(0.0), 0.0);
    }
    let bounded = clamp(value, range.x, range.y);
    let t = (bounded - range.x) / (range.y - range.x);
    // Match the legend's nearest palette-entry rounding, including endpoints.
    let color = textureLoad(palette, vec2<i32>(i32(floor(t * 255.0 + 0.5)), 0), 0);
    return RasterSample(vec4(color.rgb * color.a, color.a), select(0.0, t, color.a > 0.0));
}

fn raster_sample(uv: vec2<f32>) -> RasterSample {
    let current = sample_at(values, valid, palette, draw.uv.xy + uv * draw.uv.zw,
        draw.range, draw.scale_offset, draw.clamp_colors.x);
    let previous = sample_at(previous_values, previous_valid, previous_palette,
        draw.previous_uv.xy + uv * draw.previous_uv.zw, draw.previous_range, draw.previous_scale_offset, draw.clamp_colors.y);
    // Each snapshot normalizes in its own units/range and owns its maximum height.
    // Interpolate actual heights, not the product of separately blended factors.
    return RasterSample(
        mix(previous.color, current.color, draw.blend) * draw.opacity,
        mix(previous.height * draw.previous_extrusion_height, current.height * draw.extrusion.z, draw.blend));
}

fn raster_color(uv: vec2<f32>) -> vec4<f32> {
    return raster_sample(uv).color;
}

@fragment
fn fragment(in: Vertex) -> @location(0) vec4<f32> {
    return raster_color(in.uv);
}

// One instance per sampled pixel: top and four walls, no CPU mesh or bottom.
@vertex
fn extruded_vertex(@builtin(vertex_index) ix: u32, @builtin(instance_index) instance: u32) -> Vertex {
    let grid = draw.extrusion.xy;
    let pixel = vec2(f32(instance % u32(grid.x)), f32(instance / u32(grid.x)));
    let triangles = array<vec2<f32>, 6>(vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
        vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0));
    let corner = triangles[ix % 6u];
    let face = ix / 6u;
    var local = vec3(corner, 1.0);
    switch face {
        case 1u: { local = vec3(corner.x, 0.0, corner.y); }
        case 2u: { local = vec3(1.0, corner.x, corner.y); }
        case 3u: { local = vec3(1.0 - corner.x, 1.0, corner.y); }
        case 4u: { local = vec3(0.0, 1.0 - corner.x, corner.y); }
        default: {}
    }
    var out: Vertex;
    out.uv = (pixel + vec2(0.5)) / grid;
    let sample = raster_sample(out.uv);
    out.position = draw.matrix * vec4((pixel + local.xy) / grid, local.z * sample.height, 1.0);
    if sample.color.a <= 0.0 {
        out.position = vec4(2.0, 2.0, 2.0, 1.0);
    }
    return out;
}

@fragment
fn extruded_fragment(in: Vertex) -> @location(0) vec4<f32> {
    let color = raster_color(in.uv);
    // Transparent pixels must not hide other bars through depth writes.
    if color.a <= 0.0 { discard; }
    return color;
}
