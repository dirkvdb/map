struct Draw {
    matrix: mat4x4<f32>,
    scale: vec2<f32>,
    next_scale: vec2<f32>,
    range: vec2<f32>,
    next_range: vec2<f32>,
    parameters: vec4<f32>,
    layers: vec4<u32>,
    extrusion: vec4<f32>,
    geometry: vec4<u32>,
};

@group(0) @binding(0) var<uniform> draw: Draw;
@group(0) @binding(1) var values: texture_2d_array<f32>;
@group(0) @binding(2) var valid: texture_2d_array<f32>;
@group(0) @binding(3) var next_values: texture_2d_array<f32>;
@group(0) @binding(4) var next_valid: texture_2d_array<f32>;
@group(0) @binding(5) var palette: texture_2d<f32>;

struct RasterSample {
    color: vec4<f32>,
    height: f32,
};

// Returns a raw value and an explicit validity bit. Invalid NaNs have already
// been zeroed on upload; they cannot contaminate a valid endpoint through mix.
fn sample_band(data: texture_2d_array<f32>, mask: texture_2d_array<f32>, uv: vec2<f32>, layer: u32) -> vec2<f32> {
    let size = vec2<i32>(textureDimensions(data));
    let pixel = clamp(vec2<i32>(uv * vec2<f32>(size)), vec2(0), size - vec2(1));
    return vec2(textureLoad(data, pixel, i32(layer), 0).r, textureLoad(mask, pixel, i32(layer), 0).r);
}

fn sample_color(value: f32, range: vec2<f32>, alpha: f32) -> RasterSample {
    if draw.layers.z == 0u && (value < range.x || value > range.y) {
        return RasterSample(vec4(0.0), 0.0);
    }
    let normalized = clamp((value - range.x) / max(range.y - range.x, 1e-30), 0.0, 1.0);
    let color = textureLoad(palette, vec2<i32>(i32(floor(normalized * 255.0 + 0.5)), 0), 0);
    let opacity = color.a * alpha * draw.parameters.y;
    return RasterSample(vec4(color.rgb * opacity, opacity), normalized * draw.extrusion.z);
}

fn sample_endpoint(band: vec2<f32>, scale: vec2<f32>, range: vec2<f32>) -> RasterSample {
    if band.y > 0.5 {
        return sample_color(band.x * scale.x + scale.y, range, 1.0);
    }
    return RasterSample(vec4(0.0), 0.0);
}

fn raster_sample(uv: vec2<f32>) -> RasterSample {
    let t = draw.parameters.x;
    if t <= 0.0 {
        return sample_endpoint(sample_band(values, valid, uv, draw.layers.x), draw.scale, draw.range);
    }
    if t >= 1.0 {
        return sample_endpoint(sample_band(next_values, next_valid, uv, draw.layers.y), draw.next_scale, draw.next_range);
    }
    let a = sample_band(values, valid, uv, draw.layers.x);
    let b = sample_band(next_values, next_valid, uv, draw.layers.y);
    return sample_temporal(a, b, t);
}

fn sample_temporal(a: vec2<f32>, b: vec2<f32>, t: f32) -> RasterSample {
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
    return sample_color(value, range, alpha);
}

fn raster_color(uv: vec2<f32>) -> vec4<f32> {
    return raster_sample(uv).color;
}
