// A small numeric bitmap font keeps debug labels independent of system fonts
// and readable at a fixed logical-pixel size on the native raster surface.
// Glyphs are five columns of seven bits: digits, slash, R, and space.
const DEBUG_GLYPHS = array<array<u32, 5>, 13>(
    array(0x3eu, 0x51u, 0x49u, 0x45u, 0x3eu),
    array(0x00u, 0x42u, 0x7fu, 0x40u, 0x00u),
    array(0x42u, 0x61u, 0x51u, 0x49u, 0x46u),
    array(0x21u, 0x41u, 0x45u, 0x4bu, 0x31u),
    array(0x18u, 0x14u, 0x12u, 0x7fu, 0x10u),
    array(0x27u, 0x45u, 0x45u, 0x45u, 0x39u),
    array(0x3cu, 0x4au, 0x49u, 0x49u, 0x30u),
    array(0x01u, 0x71u, 0x09u, 0x05u, 0x03u),
    array(0x36u, 0x49u, 0x49u, 0x49u, 0x36u),
    array(0x06u, 0x49u, 0x49u, 0x29u, 0x1eu),
    array(0x20u, 0x10u, 0x08u, 0x04u, 0x02u),
    array(0x7fu, 0x09u, 0x19u, 0x29u, 0x46u),
    array(0x00u, 0x00u, 0x00u, 0x00u, 0x00u),
);

fn debug_digits(value: u32) -> u32 {
    var digits = 1u;
    var remaining = value;
    while remaining >= 10u {
        remaining /= 10u;
        digits += 1u;
    }
    return digits;
}

fn debug_digit(value: u32, count: u32, ix: u32) -> u32 {
    var remaining = value;
    for (var i = ix + 1u; i < count; i += 1u) {
        remaining /= 10u;
    }
    return remaining % 10u;
}

fn debug_glyph(ix: u32, digits: vec3<u32>) -> u32 {
    if ix == 0u { return 11u; } // R identifies the custom raster layer.
    if ix == 1u { return 12u; }
    if ix < 2u + digits.x { return debug_digit(draw.tile_id.x, digits.x, ix - 2u); }
    if ix == 2u + digits.x { return 10u; }
    if ix < 3u + digits.x + digits.y { return debug_digit(draw.tile_id.y, digits.y, ix - 3u - digits.x); }
    if ix == 3u + digits.x + digits.y { return 10u; }
    return debug_digit(draw.tile_id.z, digits.z, ix - 4u - digits.x - digits.y);
}

@fragment
fn debug_fragment(in: Vertex) -> @location(0) vec4<f32> {
    let pixel = in.uv * draw.tile_size;
    let digits = vec3(debug_digits(draw.tile_id.x), debug_digits(draw.tile_id.y), debug_digits(draw.tile_id.z));
    let parts = vec3(3u + digits.x, 1u + digits.y, digits.z);
    let total = parts.x + parts.y + parts.z;
    // Wrap long IDs rather than shrinking the glyphs in small on-screen tiles.
    let wrapped = f32(total) * 12.0 + 16.0 > draw.tile_size;
    let columns = select(total, max(parts.x, max(parts.y, parts.z)), wrapped);
    let rows = select(1u, 3u, wrapped);
    let label_size = vec2(f32(columns) * 12.0 + 8.0, f32(rows) * 18.0 + 6.0);
    let label_pixel = pixel - vec2(6.0);
    if all(label_pixel >= vec2(0.0)) && all(label_pixel < label_size) {
        let cell = vec2<i32>(floor((label_pixel - vec2(4.0, 3.0)) / 2.0));
        let row = u32(max(cell.y, 0)) / 9u;
        let column = u32(max(cell.x, 0)) / 6u;
        if all(cell >= vec2(0)) && row < rows {
            var offset = 0u;
            var count = total;
            if wrapped {
                count = parts[row];
                if row > 0u { offset = parts.x; }
                if row > 1u { offset += parts.y; }
            }
            if column < count && cell.x % 6 < 5 && cell.y % 9 < 7 {
                let glyph = DEBUG_GLYPHS[debug_glyph(offset + column, digits)];
                if (glyph[u32(cell.x % 6)] & (1u << u32(cell.y % 9))) != 0u {
                    return vec4(0.0, 0.0, 0.0, 1.0);
                }
            }
        }
        return vec4(0.92, 0.92, 0.92, 0.92);
    }
    if min(min(pixel.x, pixel.y), min(draw.tile_size - pixel.x, draw.tile_size - pixel.y)) < 1.5 {
        return vec4(1.0, 0.8, 0.0, 1.0);
    }
    return vec4(0.0);
}
