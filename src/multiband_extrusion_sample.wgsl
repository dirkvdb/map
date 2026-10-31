// Compose after multiband_sample.wgsl and before raster_extrusion.wgsl.
fn extrusion_band(data: texture_2d_array<f32>, mask: texture_2d_array<f32>, uv: vec2<f32>, layer: u32, average: bool) -> vec2<f32> {
    if !average { return sample_band(data, mask, uv, layer); }
    let grid = vec2<u32>(draw.extrusion.xy);
    let base = (vec2<u32>(floor(uv * vec2<f32>(grid))) / vec2(2u)) * vec2(2u);
    var total = 0.0;
    var weight = 0.0;
    for (var i = 0u; i < 4u; i += 1u) {
        let logical = base + vec2(i & 1u, i >> 1u);
        if any(logical >= grid) { continue; }
        let logical_uv = (vec2<f32>(logical) + vec2(0.5)) / vec2<f32>(grid);
        let band = sample_band(data, mask, logical_uv, layer);
        if band.y > 0.5 {
            // Scale before summing, so four finite values cannot overflow.
            total += band.x * 0.25;
            weight += 0.25;
        }
    }
    if weight == 0.0 { return vec2(0.0); }
    // Nonempty means are valid endpoints, regardless of their display range.
    return vec2(total / weight, 1.0);
}

fn extrusion_sample(uv: vec2<f32>) -> RasterSample {
    // Bit 0 averages blend=0 (current); bit 1 averages blend=1 (next).
    let flags = draw.geometry.w & 3u;
    if flags == 0u && draw.outgoing.w <= 0.5 { return raster_sample(uv); }
    let t = draw.parameters.x;
    if t <= 0.0 && (draw.parameters.w < 0.5 || draw.outgoing.x <= 0.0) {
        let a = extrusion_band(values, valid, uv, draw.layers.x, (flags & 1u) != 0u);
        return sample_endpoint(a, draw.scale, draw.range);
    }
    if t >= 1.0 {
        let b = extrusion_band(next_values, next_valid, uv, draw.layers.y, (flags & 2u) != 0u);
        return sample_endpoint(b, draw.next_scale, draw.next_range);
    }
    let a = extrusion_band(values, valid, uv, draw.layers.x, (flags & 1u) != 0u);
    let b = extrusion_band(next_values, next_valid, uv, draw.layers.y, (flags & 2u) != 0u);
    var c = vec2(0.0);
    if draw.parameters.w > 0.5 && draw.outgoing.x > 0.0 {
        c = extrusion_band(outgoing_values, outgoing_valid, uv, u32(draw.outgoing.y), draw.outgoing.w > 0.5);
    }
    return sample_transition(a, b, c, t);
}
