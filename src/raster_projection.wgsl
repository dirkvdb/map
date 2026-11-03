// Prepend to each raster shader; its Draw uniform supplies `projection` and the
// existing tile-local Mercator `matrix` (whose elevation input is in meters).
struct ProjectionUniform {
    globe_matrix: mat4x4<f32>,
    clipping_plane: vec4<f32>,
    tile_mercator: vec4<f32>,
    parameters: vec4<f32>,
};

fn raster_projection_active() -> bool {
    return draw.projection.parameters.w > 0.0 && draw.projection.parameters.x > 0.0;
}

fn raster_vertex_uv(ix: u32) -> vec2<f32> {
    let corners = array<vec2<f32>, 6>(
        vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
        vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0),
    );
    // Legacy fixtures upload zeros; both Rust and WGSL interpret that as one quad.
    let sections = u32(max(draw.projection.parameters.z, 1.0));
    let cell = ix / 6u;
    return (vec2<f32>(f32(cell % sections), f32(cell / sections)) + corners[ix % 6u]) / f32(sections);
}

fn raster_globe_sphere(uv: vec2<f32>, height: f32) -> vec3<f32> {
    let pi = 3.141592653589793;
    let mercator = draw.projection.tile_mercator.xy + uv * draw.projection.tile_mercator.zw;
    let longitude = mercator.x * (2.0 * pi) + pi;
    // Match native's half-angle form, retaining float32 precision at the equator.
    let t = exp(pi - mercator.y * (2.0 * pi));
    let t2 = t * t;
    let cos_latitude = 2.0 * t / (t2 + 1.0);
    let sphere = vec3(sin(longitude) * cos_latitude, (t2 - 1.0) / (t2 + 1.0), cos(longitude) * cos_latitude);
    return sphere * (1.0 + height / 6371008.8);
}

fn project_surface(uv: vec2<f32>) -> vec4<f32> {
    let flat = draw.matrix * vec4(uv, 0.0, 1.0);
    if !raster_projection_active() { return flat; }
    let sphere = raster_globe_sphere(uv, 0.0);
    var globe = draw.projection.globe_matrix * vec4(sphere, 1.0);
    let plane = draw.projection.clipping_plane;
    globe.z = (1.0 - (dot(sphere, plane.xyz) + plane.w)) * globe.w - draw.projection.parameters.y;
    let transition = draw.projection.parameters.x;
    if transition > 0.999 { return globe; }
    // Surface depth intentionally differs from a four-component matrix blend.
    let xyw = mix(flat.xyw, globe.xyw, transition);
    let z = mix(0.0, globe.z, clamp((transition - 0.2) / 0.8, 0.0, 1.0));
    return vec4(xyw.xy, z, xyw.z);
}

fn project_volume(uv: vec2<f32>, height: f32) -> vec4<f32> {
    let flat = draw.matrix * vec4(uv, height, 1.0);
    if !raster_projection_active() { return flat; }
    let globe = draw.projection.globe_matrix * vec4(raster_globe_sphere(uv, height), 1.0);
    var position = globe;
    let transition = draw.projection.parameters.x;
    if transition <= 0.999 { position = mix(flat, globe, transition); }
    // Private reversed depth (1/W, clear 0, Greater) avoids independent globe Z/W
    // rounding and forward-depth quantization choosing distant walls over tops.
    // Encode after blending, with a near distance of 1 and no finite far plane.
    position.z = 1.0;
    return position;
}

fn globe_horizon(uv: vec2<f32>, height: f32) -> f32 {
    if !raster_projection_active() { return 1.0; }
    let plane = draw.projection.clipping_plane;
    return dot(raster_globe_sphere(uv, height), plane.xyz) + plane.w;
}

fn globe_hidden(horizon: f32) -> bool {
    // Raster volumes use private depth to avoid fighting the native planet mesh.
    // Never hard-clip the horizon during the homogeneous Mercator/globe blend.
    return raster_projection_active() && draw.projection.parameters.x > 0.999 && horizon < 0.0;
}
