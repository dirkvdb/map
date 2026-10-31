//! CPU-only extrusion draw planning; source grids and physical heights stay unchanged.

use std::cmp::Ordering;

use maplibre_native::WgpuLayerParameters;

use crate::raster_tiles::VisibleTile;

const CLIP_EPSILON: f64 = 1e-6;
const MAX_STRIDE: u32 = 16;
const TARGET_PIXELS: f64 = 0.5;

/// Plan `[stride_x, stride_y, order_flags, 0]` for a column-major tile matrix.
///
/// `extrusion` is `[source_nx, source_ny, maximum_height_meters, enabled]`.
/// Strides are powers of two, at most 16 and no larger than their source axis.
/// Only subpixel cells are coarsened, keeping the bounded coarsened footprint at
/// most 0.5 logical pixels. DPI is not available, so physical pixels are not used.
/// Invalid inputs, nonpositive clip W, or a volume touching the back of the near
/// plane retain full detail. No grid or physical height is modified.
///
/// Opaque draws get approximate front-to-back order from clip-depth derivatives
/// at the volume center: bit 0 reverses X, bit 1 reverses Y, and bit 2 selects
/// X-major (column-major, Y-fast) traversal. Transparent draws keep flags zero.
pub fn plan(matrix: &[[f32; 4]; 4], viewport: [f64; 2], extrusion: [f32; 4], opaque: bool) -> [u32; 4] {
    let matrix = matrix.map(|column| column.map(f64::from));
    if !valid_inputs(&matrix, viewport, extrusion) {
        return [1, 1, 0, 0];
    }
    let height = f64::from(extrusion[2]);
    let corners: [[f64; 4]; 8] =
        std::array::from_fn(|corner| clip_position(&matrix, [(corner & 1) as f64, ((corner >> 1) & 1) as f64, (corner >> 2) as f64 * height]));
    // W and the near-plane equation are affine, so their extrema occur at corners.
    if corners.iter().any(|clip| clip[3] <= CLIP_EPSILON || clip[2] < 0.0) {
        return [1, 1, 0, 0];
    }
    let min_w = corners.iter().map(|clip| clip[3]).fold(f64::INFINITY, f64::min);
    let strides: [u32; 2] = std::array::from_fn(|axis| {
        let pixels = projected_cell_bound(matrix[axis], &corners, viewport, min_w) / f64::from(extrusion[axis]);
        choose_stride(pixels, extrusion[axis] as u32)
    });
    let flags = if opaque { order_flags(&matrix, height) } else { 0 };
    [strides[0], strides[1], flags, 0]
}

/// Add optional 2×2 averaging without changing the source grid or physical height.
/// Bits 0/1 select the blend-zero/one endpoint. Mixed-mode fades retain the
/// finer geometry until both active endpoints can share the combined footprint.
pub fn with_averaging(mut geometry: [u32; 4], flags: u32, blend: f32) -> [u32; 4] {
    let flags = flags & 3;
    let combined = if blend <= 0.0 {
        flags & 1 != 0
    } else if blend >= 1.0 {
        flags & 2 != 0
    } else {
        flags == 3
    };
    if combined {
        geometry[0] = geometry[0].max(2);
        geometry[1] = geometry[1].max(2);
    }
    geometry[3] = flags;
    geometry
}

fn valid_inputs(matrix: &[[f64; 4]; 4], viewport: [f64; 2], extrusion: [f32; 4]) -> bool {
    matrix.iter().flatten().all(|value| value.is_finite())
        && viewport.iter().all(|value| value.is_finite() && *value > 0.0)
        && extrusion.iter().all(|value| value.is_finite())
        && extrusion[0] >= 1.0
        && extrusion[1] >= 1.0
        && extrusion[2] >= 0.0
        && extrusion[3] > 0.0
}

fn clip_position(matrix: &[[f64; 4]; 4], position: [f64; 3]) -> [f64; 4] {
    std::array::from_fn(|row| matrix[0][row] * position[0] + matrix[1][row] * position[1] + matrix[2][row] * position[2] + matrix[3][row])
}

fn projected_cell_bound(axis: [f64; 4], corners: &[[f64; 4]; 8], viewport: [f64; 2], min_w: f64) -> f64 {
    // The quotient-rule numerator is affine; its screen-space norm is convex.
    // Its corner maximum / minimum W² bounds the derivative throughout the box,
    // unlike projecting an axis at only the tile center or measuring its endpoints.
    let numerator = corners
        .iter()
        .map(|clip| {
            let dx = (axis[0] * clip[3] - clip[0] * axis[3]) * (viewport[0] * 0.5);
            let dy = (axis[1] * clip[3] - clip[1] * axis[3]) * (viewport[1] * 0.5);
            dx.hypot(dy)
        })
        .fold(0.0, f64::max);
    numerator / min_w.powi(2)
}

fn choose_stride(pixels: f64, grid: u32) -> u32 {
    if !pixels.is_finite() || pixels < 0.0 {
        return 1;
    }
    let mut stride = 1;
    while stride < MAX_STRIDE && stride * 2 <= grid && pixels * f64::from(stride * 2) <= TARGET_PIXELS {
        stride *= 2;
    }
    stride
}

fn order_flags(matrix: &[[f64; 4]; 4], height: f64) -> u32 {
    let center = clip_position(matrix, [0.5, 0.5, height * 0.5]);
    // The positive W² denominator is shared, so only the numerators are needed.
    let derivative: [f64; 2] = std::array::from_fn(|axis| matrix[axis][2] * center[3] - center[2] * matrix[axis][3]);
    u32::from(derivative[0] < 0.0) | (u32::from(derivative[1] < 0.0) << 1) | (u32::from(derivative[0].abs() > derivative[1].abs()) << 2)
}

/// Count coarsened cells using ceiling division, including partial edge cells.
/// Zero strides mean one for legacy uniforms. Unrepresentable counts saturate.
pub fn instance_count(extrusion: [f32; 4], geometry: [u32; 4]) -> u32 {
    let nx = (extrusion[0] as u32).div_ceil(geometry[0].max(1));
    let ny = (extrusion[1] as u32).div_ceil(geometry[1].max(1));
    nx.saturating_mul(ny)
}

/// Sort unwrapped ground centers near-to-far by clip Z/W, using f64 throughout.
/// Equal depths use `(z, y, x)` ties. Invalid depths follow valid depths and retain
/// their input order; a wholly invalid projection leaves the slice unchanged.
/// No tile matrix or temporary depth collection is built for comparisons.
pub fn sort_tiles(parameters: &WgpuLayerParameters, tiles: &mut [VisibleTile]) {
    let world = 512.0 * parameters.zoom.exp2();
    tiles.sort_by(
        |a, b| match (tile_depth(&parameters.projection, world, a), tile_depth(&parameters.projection, world, b)) {
            (Some(a_depth), Some(b_depth)) => a_depth
                .total_cmp(&b_depth)
                .then_with(|| a.z.cmp(&b.z))
                .then_with(|| a.y.cmp(&b.y))
                .then_with(|| a.x.cmp(&b.x)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        },
    );
}

fn tile_depth(projection: &[f64; 16], world: f64, tile: &VisibleTile) -> Option<f64> {
    let size = world * (-f64::from(tile.z)).exp2();
    if !size.is_finite() || size <= 0.0 {
        return None;
    }
    let x = (tile.x as f64 + 0.5) * size;
    let y = (f64::from(tile.y) + 0.5) * size;
    let z = projection[2] * x + projection[6] * y + projection[14];
    let w = projection[3] * x + projection[7] * y + projection[15];
    let depth = z / w;
    (w > CLIP_EPSILON && w.is_finite() && depth.is_finite()).then_some(depth)
}

#[cfg(test)]
#[path = "raster_extrusion_geometry_tests.rs"]
mod tests;
