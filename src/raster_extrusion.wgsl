struct Vertex {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(flat) color: vec4<f32>,
    @location(1) local_position: vec3<f32>,
};

// One instance per displayed cell: eight indexed corners, top and four walls.
@vertex
fn extruded_vertex(@builtin(vertex_index) ix: u32, @builtin(instance_index) instance: u32) -> Vertex {
    let grid = vec2<u32>(draw.extrusion.xy);
    let stride = max(draw.geometry.xy, vec2(1u));
    let cells = (grid + stride - vec2(1u)) / stride;
    var cell = vec2(instance % cells.x, instance / cells.x);
    if (draw.geometry.z & 4u) != 0u {
        cell = vec2(instance / cells.y, instance % cells.y);
    }
    if (draw.geometry.z & 1u) != 0u { cell.x = cells.x - 1u - cell.x; }
    if (draw.geometry.z & 2u) != 0u { cell.y = cells.y - 1u - cell.y; }
    let start = vec2<f32>(cell * stride);
    let end = vec2<f32>(min((cell + vec2(1u)) * stride, grid));
    let size = vec2<f32>(grid);
    // Bits 0/1 choose x/y; bit 2 chooses ground (0) or sampled height (1).
    let local = vec3(f32(ix & 1u), f32((ix >> 1u) & 1u), f32((ix >> 2u) & 1u));
    // Aligned nearest samples keep LOD deterministic, with no raw-tile reload.
    let sample = extrusion_sample((start + end) * 0.5 / size);
    var out: Vertex;
    out.position = draw.matrix * vec4(mix(start, end, local.xy) / size, local.z * sample.height, 1.0);
    out.color = sample.color;
    out.local_position = local;
    if sample.color.a <= 0.0 {
        out.position = vec4(2.0, 2.0, 2.0, 1.0);
    }
    return out;
}

@fragment
fn extruded_fragment(in: Vertex) -> @location(0) vec4<f32> {
    // Derivatives recover a constant face normal without duplicating the eight
    // shared corners for per-face vertex normals. Evaluate before any discard.
    let normal = normalize(cross(dpdx(in.local_position), dpdy(in.local_position)));
    // Wrapped northwest light keeps every wall readable and distinct, including
    // the sides facing away from the light. Upward faces retain exact LUT colors.
    let light = vec3(-0.45, -0.7, 1.0);
    let illumination = clamp(0.6 + 0.4 * dot(normal, light), 0.0, 1.0);
    // Shade premultiplied RGB only; opacity and nodata depth behavior are unchanged.
    if in.color.a <= 0.0 { discard; }
    return vec4(in.color.rgb * illumination, in.color.a);
}
