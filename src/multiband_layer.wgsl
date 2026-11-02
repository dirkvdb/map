struct Vertex {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vertex(@builtin(vertex_index) ix: u32) -> Vertex {
    let uv = raster_vertex_uv(ix);
    var out: Vertex;
    out.position = project_surface(uv);
    out.uv = uv;
    return out;
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
