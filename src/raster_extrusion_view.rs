//! Extrusion-only coverage. X/Y use unwrapped Mercator coordinates; heights use meters.

use std::collections::HashSet;

use geo::{Coordinate, LatLonBounds};
use maplibre_native::WgpuLayerParameters;

use super::{VisibleTile, extrusion_height, intersects_bounds, normalized_mercator, selected_zoom};
use crate::raster_source::RasterSource;

// Invalid/pathological projections must not turn into unlimited world-copy or tile scans.
const MAX_WORLD_COPIES: f64 = 1024.0;
const MAX_WORLD_OFFSET: f64 = 1_000_000.0;
const MAX_VISITS: usize = 262_144;
const PLANE_EPSILON: f64 = 1e-10;
type Plane = [f64; 4];

/// Extruded coverage on the finest source-selected grid, nearest the camera first.
///
/// `sources` pairs each source with its maximum *style* height (zero when disabled),
/// including both slots during a fade. Heights are fixed by each source-selected
/// zoom and the projection's actual pixels-per-meter scale, independently of the
/// coverage budget, cached ancestors, and rendered grid. Coarsening changes only
/// coverage, never bar height. At zoom zero the budget may still be exceeded;
/// unwrapped copies do not count against the canonical budget.
///
/// Unusable or unbounded projections fall back to a finite camera-centered viewport
/// (at most 1024 world copies). A fixed traversal budget also permits conservative
/// coarsening for pathological inputs. Flat coverage APIs are unaffected.
pub fn visible_tiles(parameters: &WgpuLayerParameters, sources: &[(&RasterSource, f32)], tile_limit: usize) -> Vec<VisibleTile> {
    let Some(mut z) = sources.iter().map(|(source, _)| selected_zoom(parameters, source)).max() else {
        return Vec::new();
    };
    let bounds: Vec<LatLonBounds> = sources.iter().map(|(source, _)| source.metadata().bounds.into()).collect();
    let view = View::new(parameters, maximum_height(parameters, sources));
    loop {
        if let Some(mut tiles) = view.coverage(z, &bounds, tile_limit) {
            sort_tiles(&mut tiles, view.origin);
            return tiles;
        }
        z -= 1;
    }
}

/// Maximum projection-input height in meters across all active/fading sources.
/// The shared conversion is independent of coverage zoom and cache contents.
pub fn maximum_height(parameters: &WgpuLayerParameters, sources: &[(&RasterSource, f32)]) -> f64 {
    sources
        .iter()
        .map(|(source, height)| extrusion_height::maximum(parameters, source, *height))
        .fold(0.0, f64::max)
}

/// Conservatively test a retained tile's full [ground, `maxheight`] AABB.
/// `maxheight` is physical meters, unlike the style heights passed to [`visible_tiles`].
/// Non-finite projection/height or unsupported zoom inputs fail open; this helper never enumerates tiles.
pub fn tile_visible(parameters: &WgpuLayerParameters, tile: VisibleTile, maxheight: f64) -> bool {
    if !(0..=22).contains(&tile.z) || !maxheight.is_finite() {
        return true;
    }
    let origin = camera_origin(parameters);
    let planes = clip_planes(parameters, origin, maxheight.max(0.0));
    planes.is_none_or(|planes| box_visible(&planes, origin, tile))
}

struct View {
    origin: [f64; 2],
    bounds: [f64; 4],
    planes: Option<[Plane; 6]>,
}

impl View {
    fn new(parameters: &WgpuLayerParameters, height: f64) -> Self {
        let origin = camera_origin(parameters);
        let planes = clip_planes(parameters, origin, height);
        let bounds = planes.as_ref().and_then(|planes| slab_bounds(planes, origin));
        match bounds.filter(|bounds| bounded_copies(*bounds)) {
            Some(bounds) => Self { origin, bounds, planes },
            None => Self {
                origin,
                bounds: fallback_bounds(parameters, origin),
                planes: None,
            },
        }
    }

    fn coverage(&self, z: i32, bounds: &[LatLonBounds], tile_limit: usize) -> Option<Vec<VisibleTile>> {
        let [left, top, right, bottom] = self.bounds;
        if left >= right || top >= bottom {
            return Some(Vec::new());
        }
        let mut pending: Vec<_> = (left.floor() as i64..right.ceil() as i64).map(|x| VisibleTile { z: 0, x, y: 0 }).collect();
        let mut canonical = HashSet::new();
        let mut tiles = Vec::new();
        let mut visits = 0;
        while let Some(tile) = pending.pop() {
            visits += 1;
            if visits > MAX_VISITS {
                return None;
            }
            if !self.intersects(tile, bounds) {
                continue;
            }
            if tile.z < z {
                push_children(&mut pending, tile);
                continue;
            }
            canonical.insert(tile.canonical());
            if z > 0 && canonical.len() > tile_limit {
                return None;
            }
            tiles.push(tile);
        }
        Some(tiles)
    }

    fn intersects(&self, tile: VisibleTile, bounds: &[LatLonBounds]) -> bool {
        let n = f64::from(1_u32 << tile.z);
        let x = tile.x as f64 / n;
        let y = f64::from(tile.y) / n;
        let [left, top, right, bottom] = self.bounds;
        x < right
            && x + 1.0 / n > left
            && y < bottom
            && y + 1.0 / n > top
            && bounds.iter().any(|bounds| intersects_bounds(tile, *bounds))
            && self.planes.as_ref().is_none_or(|planes| box_visible(planes, self.origin, tile))
    }
}

fn push_children(pending: &mut Vec<VisibleTile>, tile: VisibleTile) {
    for y in 0..2 {
        for x in 0..2 {
            pending.push(VisibleTile {
                z: tile.z + 1,
                x: tile.x * 2 + x,
                y: tile.y * 2 + y,
            });
        }
    }
}

fn sort_tiles(tiles: &mut [VisibleTile], origin: [f64; 2]) {
    tiles.sort_by(|a, b| {
        let distance = |tile: &VisibleTile| {
            let n = f64::from(1_u32 << tile.z);
            (tile.x as f64 + 0.5 - origin[0] * n).hypot(f64::from(tile.y) + 0.5 - origin[1] * n)
        };
        distance(a).total_cmp(&distance(b)).then(a.y.cmp(&b.y)).then(a.x.cmp(&b.x))
    });
}

fn camera_origin(parameters: &WgpuLayerParameters) -> [f64; 2] {
    let point = normalized_mercator(Coordinate::latlon(parameters.latitude, parameters.longitude));
    let finite = |value: f64| if value.is_finite() { value } else { 0.5 };
    [finite(point.x()).clamp(-MAX_WORLD_OFFSET, MAX_WORLD_OFFSET), finite(point.y())]
}

fn fallback_bounds(parameters: &WgpuLayerParameters, [x, y]: [f64; 2]) -> [f64; 4] {
    let world = 512.0 * parameters.zoom.exp2();
    let half_span = |pixels: f64| {
        let span = pixels / (2.0 * world);
        if span.is_finite() && span >= 0.0 {
            span.min((MAX_WORLD_COPIES - 2.0) / 2.0)
        } else {
            0.5
        }
    };
    let dx = half_span(parameters.width);
    let dy = half_span(parameters.height);
    [x - dx, (y - dy).max(0.0), x + dx, (y + dy).min(1.0)]
}

fn bounded_copies([left, top, right, bottom]: [f64; 4]) -> bool {
    [left, top, right, bottom].iter().all(|value| value.is_finite())
        && right.ceil() - left.floor() <= MAX_WORLD_COPIES
        && left.abs().max(right.abs()) <= MAX_WORLD_OFFSET + MAX_WORLD_COPIES
}

fn clip_planes(parameters: &WgpuLayerParameters, origin: [f64; 2], height: f64) -> Option<[Plane; 6]> {
    let world = 512.0 * parameters.zoom.exp2();
    if !world.is_finite() || world <= 0.0 || !height.is_finite() {
        return None;
    }
    let m = &parameters.projection;
    // Camera-relative normalized X/Y preserve precision; local Z runs from 0 to 1.
    let rows: [Plane; 4] = std::array::from_fn(|r| {
        [
            m[r] * world,
            m[4 + r] * world,
            m[8 + r] * height,
            m[r] * world * origin[0] + m[4 + r] * world * origin[1] + m[12 + r],
        ]
    });
    // WebGPU clip volume: -w <= x,y <= w, 0 <= z <= w.
    let mut planes = [rows[2]; 6];
    for (index, (row, sign)) in [(0, 1.0), (0, -1.0), (1, 1.0), (1, -1.0), (2, -1.0)].into_iter().enumerate() {
        planes[index] = std::array::from_fn(|axis| rows[3][axis] + sign * rows[row][axis]);
    }
    for plane in &mut planes {
        if !plane.iter().all(|value| value.is_finite()) {
            return None;
        }
        let scale = plane.iter().copied().map(f64::abs).fold(0.0, f64::max);
        if scale > 0.0 {
            *plane = plane.map(|value| value / scale);
        }
    }
    Some(planes)
}

fn box_visible(planes: &[Plane; 6], origin: [f64; 2], tile: VisibleTile) -> bool {
    let n = f64::from(1_u32 << tile.z);
    let corners: [[f64; 3]; 8] = std::array::from_fn(|corner| {
        [
            (tile.x as f64 + (corner & 1) as f64) / n - origin[0],
            (f64::from(tile.y) + ((corner >> 1) & 1) as f64) / n - origin[1],
            (corner >> 2) as f64,
        ]
    });
    !planes.iter().any(|plane| {
        corners
            .iter()
            .all(|p| plane[0] * p[0] + plane[1] * p[1] + plane[2] * p[2] + plane[3] < -PLANE_EPSILON)
    })
}

/// Interval enclosure of the intersection of all clip planes and the height slab.
/// Unlike corner ray/ground intersections, this also works across the near plane
/// and with the camera inside the slab. The map's finite Y range closes the slab.
fn slab_bounds(planes: &[Plane; 6], origin: [f64; 2]) -> Option<[f64; 4]> {
    let mut ranges = [[f64::NEG_INFINITY, f64::INFINITY], [-origin[1], 1.0 - origin[1]], [0.0, 1.0]];
    // Each pass only tightens a conservative enclosure; convergence is not required.
    for _ in 0..4 {
        constrain_axis(&mut ranges, 0, planes);
        if !ranges[0].iter().all(|value| value.is_finite()) {
            return None;
        }
        constrain_axis(&mut ranges, 1, planes);
        if ranges.iter().any(|[min, max]| min > max) {
            return Some([0.0; 4]);
        }
    }
    Some([
        ranges[0][0] + origin[0],
        ranges[1][0] + origin[1],
        ranges[0][1] + origin[0],
        ranges[1][1] + origin[1],
    ])
}

fn constrain_axis(ranges: &mut [[f64; 2]; 3], axis: usize, planes: &[Plane; 6]) {
    for plane in planes {
        let coefficient = plane[axis];
        if coefficient == 0.0 {
            continue;
        }
        let rest: f64 = (0..3)
            .filter(|other| *other != axis)
            .map(|other| (plane[other] * ranges[other][0]).max(plane[other] * ranges[other][1]))
            .sum();
        let bound = -(rest + plane[3] + PLANE_EPSILON) / coefficient;
        if coefficient > 0.0 {
            ranges[axis][0] = ranges[axis][0].max(bound);
        } else {
            ranges[axis][1] = ranges[axis][1].min(bound);
        }
    }
}

#[cfg(test)]
#[path = "raster_extrusion_view_tests.rs"]
mod tests;
