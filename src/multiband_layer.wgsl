struct Draw {
    matrix: mat4x4<f32>,
    scale: vec2<f32>,
    next_scale: vec2<f32>,
    range: vec2<f32>,
    next_range: vec2<f32>,
    parameters: vec4<f32>,
    layers: vec4<u32>,
    extrusion: vec4<f32>,
};

@group(0) @binding(0) var<uniform> draw: Draw;
@group(0) @binding(1) var values: texture_2d_array<f32>;
@group(0) @binding(2) var valid: texture_2d_array<f32>;
@group(0) @binding(3) var next_values: texture_2d_array<f32>;
@group(0) @binding(4) var next_valid: texture_2d_array<f32>;
@group(0) @binding(5) var palette: texture_2d<f32>;

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

// Returns a raw value and an explicit validity bit. Invalid NaNs have already
// been zeroed on upload; they cannot contaminate a valid endpoint through mix.
fn sample_band(data: texture_2d_array<f32>, mask: texture_2d_array<f32>, uv: vec2<f32>, layer: u32) -> vec2<f32> {
    let size = vec2<i32>(textureDimensions(data));
    let pixel = clamp(vec2<i32>(uv * vec2<f32>(size)), vec2(0), size - vec2(1));
    return vec2(textureLoad(data, pixel, i32(layer), 0).r, textureLoad(mask, pixel, i32(layer), 0).r);
}

struct RasterSample {
    color: vec4<f32>,
    height: f32,
};

fn raster_sample(uv: vec2<f32>) -> RasterSample {
    let a = sample_band(values, valid, uv, draw.layers.x);
    let b = sample_band(next_values, next_valid, uv, draw.layers.y);
    let t = draw.parameters.x;
    let a_valid = a.y > 0.5;
    let b_valid = b.y > 0.5;
    if !a_valid && !b_valid {
        return RasterSample(vec4(0.0), 0.0);
    }

    // Apply EACH endpoint's physical conversion BEFORE temporal interpolation,
    // then perform exactly one palette lookup, never a blend of palette colors.
    let a_value = a.x * draw.scale.x + draw.scale.y;
    let b_value = b.x * draw.next_scale.x + draw.next_scale.y;
    var value = mix(a_value, b_value, t);
    // Display ranges describe the temporal frame, not pixel validity. Keeping
    // this rule for single-invalid pixels avoids a style jump at mask edges
    // and converges to the next band's custom range as t approaches one.
    let range = mix(draw.range, draw.next_range, t);
    var alpha = 1.0;
    if !a_valid {
        value = b_value;

        alpha = t;
    } else if !b_valid {
        value = a_value;

        alpha = 1.0 - t;
    }
    if draw.layers.z == 0u && (value < range.x || value > range.y) {
        return RasterSample(vec4(0.0), 0.0);
    }
    let normalized = clamp((value - range.x) / max(range.y - range.x, 1e-30), 0.0, 1.0);
    let color = textureLoad(palette, vec2<i32>(i32(floor(normalized * 255.0 + 0.5)), 0), 0);
    let opacity = color.a * alpha * draw.parameters.y;
    return RasterSample(vec4(color.rgb * opacity, opacity), normalized * draw.extrusion.z);
}

fn raster_color(uv: vec2<f32>) -> vec4<f32> {
    return raster_sample(uv).color;
}

@fragment
fn fragment(in: Vertex) -> @location(0) vec4<f32> {
    if draw.layers.w != 0u {
        let edge = min(min(in.uv.x, in.uv.y), min(1.0 - in.uv.x, 1.0 - in.uv.y));
        if edge * draw.parameters.z < 1.5 {
            let color = select(vec3(0.1, 0.8, 0.5), vec3(1.0, 0.5, 0.1), draw.layers.w == 2u);
            return vec4(color * 0.85, 0.85);
        }
    }
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
