// Compose after raster_sample.wgsl and before raster_extrusion.wgsl.
// Average raw valid values in source-grid space, before styling each snapshot.
fn extrusion_endpoint(values: texture_2d<f32>, valid: texture_2d<f32>, palette: texture_2d<f32>, uv: vec2<f32>, transform: vec4<f32>, range: vec2<f32>, scale_offset: vec2<f32>, clamp_colors: u32, average: bool) -> RasterSample {
    if !average {
        return sample_at(values, valid, palette, transform.xy + uv * transform.zw,
            range, scale_offset, clamp_colors);
    }
    let grid = vec2<u32>(draw.extrusion.xy);
    let base = (vec2<u32>(floor(uv * vec2<f32>(grid))) / vec2(2u)) * vec2(2u);
    let size = vec2<i32>(textureDimensions(values));
    var total = 0.0;
    var weight = 0.0;
    for (var i = 0u; i < 4u; i += 1u) {
        let logical = base + vec2(i & 1u, i >> 1u);
        // Do not clamp logical coordinates: odd edges have fewer contributors.
        if any(logical >= grid) { continue; }
        let logical_uv = (vec2<f32>(logical) + vec2(0.5)) / vec2<f32>(grid);
        let mapped_uv = transform.xy + logical_uv * transform.zw;
        let pixel = clamp(vec2<i32>(mapped_uv * vec2<f32>(size)), vec2(0), size - vec2(1));
        if textureLoad(valid, pixel, 0).r < 0.5 { continue; }
        // Scaling each term first prevents overflow when four finite values sum.
        // Repeated coarser texels retain their logical-pixel area weights.
        total += textureLoad(values, pixel, 0).r * 0.25;
        weight += 0.25;
    }
    if weight == 0.0 {
        return RasterSample(vec4(0.0), 0.0);
    }
    let value = (total / weight) * scale_offset.x + scale_offset.y;
    return sample_color(value, palette, range, clamp_colors);
}

fn extrusion_sample(uv: vec2<f32>) -> RasterSample {
    // Bit 0 averages blend=0 (previous); bit 1 averages blend=1 (current).
    let flags = draw.geometry.w & 3u;
    if flags == 0u { return raster_sample(uv); }
    if draw.blend <= 0.0 {
        let previous = extrusion_endpoint(previous_values, previous_valid, previous_palette,
            uv, draw.previous_uv, draw.previous_range, draw.previous_scale_offset,
            draw.clamp_colors.y, (flags & 1u) != 0u);
        return RasterSample(previous.color * draw.opacity, previous.height * draw.previous_extrusion_height);
    }
    let current = extrusion_endpoint(values, valid, palette, uv, draw.uv,
        draw.range, draw.scale_offset, draw.clamp_colors.x, (flags & 2u) != 0u);
    if draw.blend >= 1.0 {
        return RasterSample(current.color * draw.opacity, current.height * draw.extrusion.z);
    }
    let previous = extrusion_endpoint(previous_values, previous_valid, previous_palette,
        uv, draw.previous_uv, draw.previous_range, draw.previous_scale_offset,
        draw.clamp_colors.y, (flags & 1u) != 0u);
    return blend_samples(previous, current);
}
