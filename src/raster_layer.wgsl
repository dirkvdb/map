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
    return raster_color(in.uv);
}
