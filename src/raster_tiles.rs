//! Numeric tile jobs and flat Web Mercator tile coverage, independent of style.

#[path = "raster_extrusion_height.rs"]
pub(crate) mod extrusion_height;
#[path = "raster_extrusion_view.rs"]
pub mod extrusion_view;

#[cfg(test)]
#[path = "raster_zoom_test_support.rs"]
pub(crate) mod test_support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result};
use geo::{
    Coordinate, LatLonBounds, Point, Tile,
    constants::{EARTH_CIRCUMFERENCE_M, LATITUDE_MAX},
    crs::lat_lon_to_web_mercator,
};
use maplibre_native::WgpuLayerParameters;

use crate::raster_source::{NumericTile, RasterSource};

pub const MAX_PENDING_TILES: usize = 64;

struct TileJob {
    generation: u64, // Incremented whenever the visible tiles change, so stale jobs can be ignored.
    source: Arc<RasterSource>,
    tile: Tile,
    dpi: u8,
}

#[non_exhaustive]
pub struct TileResult {
    pub generation: u64,
    pub tile: Tile,
    pub data: Result<Option<NumericTile>>,
}

pub struct TileWorkers {
    jobs: async_channel::Sender<TileJob>,
    results: async_channel::Receiver<TileResult>,
    generation: Arc<AtomicU64>,
}

impl TileWorkers {
    pub fn new() -> Result<Self> {
        let (jobs, receiver) = async_channel::bounded::<TileJob>(MAX_PENDING_TILES);
        let (results, completed) = async_channel::bounded(MAX_PENDING_TILES);
        let generation = Arc::new(AtomicU64::new(0));
        for ix in 0..2 {
            let receiver = receiver.clone();
            let results = results.clone();
            let generation = generation.clone();
            std::thread::Builder::new()
                .name(format!("numeric-tiles-{ix}"))
                .spawn(move || run_tile_worker(receiver, results, generation))
                .context("Could not start numeric tile workers")?;
        }
        Ok(Self {
            jobs,
            results: completed,
            generation,
        })
    }

    pub fn reset(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn request(&self, generation: u64, source: Arc<RasterSource>, tile: Tile, dpi: u8) -> bool {
        self.jobs.try_send(TileJob { generation, source, tile, dpi }).is_ok()
    }

    pub fn completed(&self) -> impl Iterator<Item = TileResult> + '_ {
        std::iter::from_fn(|| self.results.try_recv().ok())
    }
}

fn run_tile_worker(receiver: async_channel::Receiver<TileJob>, results: async_channel::Sender<TileResult>, generation: Arc<AtomicU64>) {
    while let Ok(job) = receiver.recv_blocking() {
        if job.generation != generation.load(Ordering::Relaxed) {
            continue;
        }
        let data = job.source.numeric_tile(job.tile, job.dpi);
        if job.generation != generation.load(Ordering::Relaxed) {
            continue;
        }
        if results
            .send_blocking(TileResult {
                generation: job.generation,
                tile: job.tile,
                data,
            })
            .is_err()
        {
            break;
        }
    }
}

impl Drop for TileWorkers {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.results.close();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct VisibleTile {
    pub z: i32,
    /// Unwrapped horizontal coordinate; data requests always use canonical x.
    pub x: i64,
    pub y: i32,
}

impl VisibleTile {
    pub fn canonical(self) -> Tile {
        Tile {
            z: self.z,
            x: self.x.rem_euclid(1 << self.z) as i32,
            y: self.y,
        }
    }
}

fn normalized_mercator(coordinate: Coordinate) -> Point<f64> {
    let coordinate = Coordinate::latlon(coordinate.latitude.clamp(-LATITUDE_MAX, LATITUDE_MAX), coordinate.longitude);
    let point = lat_lon_to_web_mercator(coordinate);
    Point::new(
        0.5 + point.x() / EARTH_CIRCUMFERENCE_M,
        (0.5 - point.y() / EARTH_CIRCUMFERENCE_M).clamp(0.0, 1.0),
    )
}

fn intersects_bounds(tile: VisibleTile, bounds: LatLonBounds) -> bool {
    let tile_bounds = tile.canonical().bounds();
    if bounds.west() <= bounds.east() {
        tile_bounds.intersects(&bounds)
    } else {
        // LatLonBounds::intersects expects non-crossing bounds.
        tile_bounds.intersects(&[bounds.west(), bounds.south(), 180.0, bounds.north()].into())
            || tile_bounds.intersects(&[-180.0, bounds.south(), bounds.east(), bounds.north()].into())
    }
}

/// Conservative ground coverage from MapLibre's projection, including pitch,
/// bearing, and unwrapped world copies.
pub fn visible_tiles(parameters: &WgpuLayerParameters, source: &RasterSource) -> Vec<VisibleTile> {
    tiles_at_zoom(parameters, selected_zoom(parameters, source), &[source.metadata().bounds.into()])
}

/// Visible union of both sources' geographic bounds on the finer selected tile
/// grid, nearest the camera first. Each unwrapped tile occurs only once.
///
/// Both shader slots can sample cached ancestors on this grid without requiring
/// descendants from the coarser source or drawing overlapping parent/child quads.
/// Use [`visible_tiles`] to schedule each source's own data requests.
pub fn transition_tiles(parameters: &WgpuLayerParameters, source: &RasterSource, previous: &RasterSource) -> Vec<VisibleTile> {
    let z = selected_zoom(parameters, source).max(selected_zoom(parameters, previous));
    tiles_at_zoom(parameters, z, &[source.metadata().bounds.into(), previous.metadata().bounds.into()])
}

fn selected_zoom(parameters: &WgpuLayerParameters, source: &RasterSource) -> i32 {
    let metadata = source.metadata();
    let tile_size = f64::from(metadata.tile_size.unwrap_or(256));
    (parameters.zoom + (512.0 / tile_size).log2())
        .ceil()
        .clamp(0.0, f64::from(metadata.max_zoom.clamp(0, 22))) as i32
}

fn tiles_at_zoom(parameters: &WgpuLayerParameters, z: i32, bounds: &[LatLonBounds]) -> Vec<VisibleTile> {
    let n = (1_u32 << z) as f64;
    let world = 512.0 * parameters.zoom.exp2();
    let center = normalized_mercator(Coordinate::latlon(parameters.latitude, parameters.longitude));
    let (center_x, center_y) = (center.x(), center.y());
    // Preserve bounded flat coverage when no invertible projection is available.
    let [left, top, right, bottom] = crate::map_camera::projected_ground_bounds(&parameters.projection, parameters.zoom).unwrap_or([
        center_x - parameters.width / (2.0 * world),
        center_y - parameters.height / (2.0 * world),
        center_x + parameters.width / (2.0 * world),
        center_y + parameters.height / (2.0 * world),
    ]);
    let left = (left * n).floor() as i64;
    let right = (right * n).ceil() as i64;
    let top = (top * n).floor().max(0.0) as i32;
    let bottom = (bottom * n).ceil().min(n) as i32;
    let mut tiles = Vec::new();
    for y in top..bottom {
        for x in left..right {
            let tile = VisibleTile { z, x, y };
            if bounds.iter().any(|bounds| intersects_bounds(tile, *bounds)) {
                tiles.push(tile);
            }
        }
    }
    // Scheduling nearest the camera first makes sparse cache misses useful sooner.
    tiles.sort_by(|a, b| {
        let distance = |tile: &VisibleTile| (tile.x as f64 + 0.5 - center_x * n).hypot(f64::from(tile.y) + 0.5 - center_y * n);
        distance(a).total_cmp(&distance(b))
    });
    tiles
}

pub fn parent_uv(tile: Tile, ancestor: Tile) -> [f32; 4] {
    let scale = 1 << (tile.z - ancestor.z);
    let size = 1.0 / scale as f32;
    [(tile.x % scale) as f32 * size, (tile.y % scale) as f32 * size, size, size]
}

pub fn tile_matrix(projection: &[f64; 16], zoom: f64, tile: VisibleTile) -> [[f32; 4]; 4] {
    let world = 512.0 * zoom.exp2();
    let size = world / f64::from(1_u32 << tile.z);
    let x = tile.x as f64 * size;
    let y = f64::from(tile.y) * size;
    // Multiply in f64 before reducing to f32, preserving subpixel precision at
    // high zoom where the world origin is far from the visible raster tiles.
    std::array::from_fn(|column| {
        std::array::from_fn(|row| {
            (match column {
                0 | 1 => projection[column * 4 + row] * size,
                2 => projection[8 + row],
                _ => projection[row] * x + projection[4 + row] * y + projection[12 + row],
            }) as f32
        })
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, path::Path};

    use super::*;

    pub(super) fn coverage_source(path: &Path, [west, south, east, north]: [f64; 4]) -> Result<Arc<RasterSource>> {
        crate::raster_source::test_support::write_raster(path, 4326, 1)?;
        let mut dataset = gdal::Dataset::open_ex(
            path,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE | gdal::GdalOpenFlags::GDAL_OF_RASTER,
                ..Default::default()
            },
        )?;
        dataset.set_geo_transform(&[west, (east - west) / 64.0, 0.0, north, 0.0, (south - north) / 64.0])?;
        dataset.close()?;
        Ok(RasterSource::open(path)?.remove(0))
    }

    fn coverage_parameters() -> WgpuLayerParameters {
        WgpuLayerParameters {
            width: 512.0,
            height: 512.0,
            latitude: 0.0,
            longitude: 0.0,
            zoom: 0.0,
            projection: [0.0; 16],
            color_format: 0,
            depth_format: 0,
            depth_test: false,
        }
    }

    #[test]
    fn world_copies_share_a_numeric_tile_identity() {
        assert_eq!(VisibleTile { z: 3, x: -1, y: 2 }.canonical(), Tile { z: 3, x: 7, y: 2 });
        assert_eq!(VisibleTile { z: 3, x: 15, y: 2 }.canonical(), Tile { z: 3, x: 7, y: 2 });
    }

    #[test]
    fn mercator_projection_clamps_poles_without_wrapping_longitude() {
        for (latitude, expected_y) in [(-90.0, 1.0), (0.0, 0.5), (90.0, 0.0)] {
            let point = normalized_mercator(Coordinate::latlon(latitude, 540.0));
            assert!((point.x() - 2.0).abs() < 1e-12);
            assert!((point.y() - expected_y).abs() < 1e-12);
        }
    }

    #[test]
    fn bounds_exclude_tiles_that_only_touch_an_edge() {
        let tile = VisibleTile { z: 2, x: 1, y: 1 };
        let bounds = tile.canonical().bounds();
        assert!(intersects_bounds(tile, bounds));
        for (x, y) in [(0, 1), (2, 1), (1, 0), (1, 2)] {
            assert!(!intersects_bounds(VisibleTile { z: 2, x, y }, bounds));
        }
    }

    #[test]
    fn antimeridian_bounds_include_both_sides_and_world_copies() {
        let bounds = [170.0, -10.0, -170.0, 10.0].into();
        for x in [-1, 0, 3, 4] {
            assert!(intersects_bounds(VisibleTile { z: 2, x, y: 1 }, bounds));
        }
        for x in [1, 2] {
            assert!(!intersects_bounds(VisibleTile { z: 2, x, y: 1 }, bounds));
        }
        assert!(!intersects_bounds(VisibleTile { z: 2, x: 0, y: 0 }, bounds));
    }

    #[test]
    fn parent_fallback_samples_only_the_child_footprint() {
        let tile = Tile { z: 3, x: 7, y: 5 };
        assert_eq!(parent_uv(tile, Tile { z: 1, x: 1, y: 1 }), [0.75, 0.25, 0.25, 0.25]);
        assert_eq!(parent_uv(tile, tile), [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn coverage_excludes_outside_data_and_respects_native_maximum_zoom() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("coverage.tif");
        crate::raster_source::test_support::write_raster(&path, 4326, 1)?;
        let source = RasterSource::open(&path)?.remove(0);
        let mut parameters = WgpuLayerParameters {
            width: 512.0,
            height: 512.0,
            latitude: 0.0,
            longitude: 0.0,
            zoom: 0.0,
            projection: [0.0; 16],
            color_format: 0,
            depth_format: 0,
            depth_test: false,
        };
        let low = visible_tiles(&parameters, &source);
        assert_eq!(low.len(), 4, "Only tiles overlapping the raster should be scheduled");
        parameters.longitude = 360.0;
        let wrapped = visible_tiles(&parameters, &source);
        assert_eq!(wrapped.len(), low.len());
        for tile in &low {
            assert!(wrapped.contains(&VisibleTile {
                x: tile.x + (1 << tile.z),
                ..*tile
            }));
        }
        parameters.longitude = 90.0;
        parameters.zoom = 18.0;
        assert!(visible_tiles(&parameters, &source).is_empty());
        parameters.longitude = 0.0;
        let high = visible_tiles(&parameters, &source);
        assert!(!high.is_empty());
        assert!(high.iter().all(|tile| tile.z == source.metadata().max_zoom));
        Ok(())
    }

    #[test]
    fn transition_coverage_uses_the_finer_selected_grid_for_the_geographic_union() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let coarse = coverage_source(&dir.path().join("coarse.tif"), [-40.0, -20.0, 40.0, 20.0])?;
        let fine = coverage_source(&dir.path().join("fine.tif"), [100.0, -2.0, 104.0, 2.0])?;
        assert!(coarse.metadata().max_zoom < fine.metadata().max_zoom);
        let coarse_bounds: LatLonBounds = coarse.metadata().bounds.into();
        let fine_bounds: LatLonBounds = fine.metadata().bounds.into();

        for zoom in [0.25, 8.0] {
            let mut parameters = coverage_parameters();
            parameters.zoom = zoom;
            parameters.width = 512.0 * zoom.exp2();
            parameters.height = parameters.width;
            let coarse_z = visible_tiles(&parameters, &coarse)[0].z;
            let fine_z = visible_tiles(&parameters, &fine)[0].z;
            assert!(coarse_z < fine_z);
            let z = coarse_z.max(fine_z);
            let expected = expected_union_coverage(z, coarse_bounds, fine_bounds);
            let tiles = transition_tiles(&parameters, &coarse, &fine);
            let actual: HashSet<_> = tiles.iter().copied().collect();
            assert_eq!(actual, expected, "Coverage must be clipped geographically at the common zoom");
            assert_eq!(tiles.len(), actual.len(), "Each unwrapped quad must occur only once");
            assert!(tiles.iter().all(|tile| tile.z == z));
            assert!(tiles.iter().any(|tile| !intersects_bounds(*tile, coarse_bounds)));
            assert!(tiles.iter().any(|tile| !intersects_bounds(*tile, fine_bounds)));
            assert_eq!(tiles, transition_tiles(&parameters, &fine, &coarse));
        }
        Ok(())
    }

    fn expected_union_coverage(z: i32, coarse_bounds: LatLonBounds, fine_bounds: LatLonBounds) -> HashSet<VisibleTile> {
        let n = 1_i32 << z;
        (0..n)
            .flat_map(|y| (0..n).map(move |x| VisibleTile { z, x: i64::from(x), y }))
            .filter(|tile| {
                let bounds = tile.canonical().bounds();
                bounds.intersects(&coarse_bounds) || bounds.intersects(&fine_bounds)
            })
            .collect()
    }

    #[test]
    fn transition_coverage_deduplicates_overlapping_sources_and_preserves_single_source_coverage() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let coarse = coverage_source(&dir.path().join("coarse.tif"), [-40.0, -20.0, 40.0, 20.0])?;
        let fine = coverage_source(&dir.path().join("fine.tif"), [-2.0, -2.0, 2.0, 2.0])?;
        let mut parameters = coverage_parameters();
        parameters.zoom = 8.0;
        parameters.width = 512.0 * parameters.zoom.exp2();
        parameters.height = parameters.width;
        let fine_tiles = visible_tiles(&parameters, &fine);
        let z = fine_tiles[0].z;
        assert!(z > visible_tiles(&parameters, &coarse)[0].z);
        let tiles = transition_tiles(&parameters, &coarse, &fine);
        let unique: HashSet<_> = tiles.iter().copied().collect();
        assert_eq!(tiles.len(), unique.len(), "Overlapping bounds must not duplicate quads");
        assert!(fine_tiles.iter().all(|tile| unique.contains(tile)));
        assert!(tiles.len() > fine_tiles.len(), "The previous source's wider coverage must remain visible");
        assert_eq!(tiles, tiles_at_zoom(&parameters, z, &[coarse.metadata().bounds.into()]));
        assert_eq!(transition_tiles(&parameters, &fine, &fine), fine_tiles);
        Ok(())
    }

    #[test]
    fn transition_coverage_preserves_world_copies_camera_order_and_viewport_clipping() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let west = coverage_source(&dir.path().join("west.tif"), [-178.0, -4.0, -170.0, 4.0])?;
        let east = coverage_source(&dir.path().join("east.tif"), [170.0, -2.0, 174.0, 2.0])?;
        let mut parameters = coverage_parameters();
        parameters.width = 1536.0;
        parameters.height = 1024.0;
        parameters.longitude = 180.0;
        let tiles = transition_tiles(&parameters, &west, &east);
        let expected: HashSet<_> = (-1..5).flat_map(|x| (0..2).map(move |y| VisibleTile { z: 1, x, y })).collect();
        assert_eq!(tiles.iter().copied().collect::<HashSet<_>>(), expected);
        assert_eq!(tiles.len(), expected.len());
        let distance = |tile: VisibleTile| (tile.x as f64 + 0.5 - 2.0).hypot(f64::from(tile.y) + 0.5 - 1.0);
        assert!(tiles.windows(2).all(|pair| distance(pair[0]) <= distance(pair[1])));

        parameters.longitude += 360.0;
        let wrapped = transition_tiles(&parameters, &west, &east);
        let shifted: Vec<_> = tiles.iter().map(|tile| VisibleTile { x: tile.x + 2, ..*tile }).collect();
        assert_eq!(wrapped, shifted, "World copies retain distinct unwrapped identities and ordering");
        for (tile, copy) in tiles.iter().zip(&wrapped) {
            assert_eq!(tile.canonical(), copy.canonical());
        }

        parameters = coverage_parameters();
        parameters.zoom = 8.0;
        assert!(transition_tiles(&parameters, &west, &east).is_empty());
        parameters.longitude = 172.0;
        let east_tiles = transition_tiles(&parameters, &west, &east);
        assert!(!east_tiles.is_empty());
        assert!(east_tiles.iter().all(|tile| tile.z == east.metadata().max_zoom));
        assert!(east_tiles.iter().all(|tile| intersects_bounds(*tile, east.metadata().bounds.into())));
        Ok(())
    }

    #[test]
    fn tile_transform_preserves_small_offsets_at_high_zoom() {
        let mut projection = [0.0; 16];
        projection[0] = 1.0;
        projection[5] = 1.0;
        projection[10] = 1.0;
        projection[15] = 1.0;
        let tile = VisibleTile {
            z: 22,
            x: 2_000_000,
            y: 2_000_000,
        };
        projection[12] = -(tile.x as f64 * 512.0) + 0.25;
        projection[13] = -(f64::from(tile.y) * 512.0) + 0.5;
        let matrix = tile_matrix(&projection, 22.0, tile);
        assert_eq!((matrix[3][0], matrix[3][1]), (0.25, 0.5));
        assert_eq!((matrix[0][0], matrix[1][1]), (512.0, 512.0));
    }
}
