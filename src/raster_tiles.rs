//! Numeric tile jobs and flat Web Mercator tile coverage, independent of style.

use std::f64::consts::PI;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result};
use geo::Tile;
use maplibre_native::WgpuLayerParameters;

use crate::raster_source::{NumericTile, RasterSource};

pub const MAX_PENDING_TILES: usize = 64;

struct TileJob {
    generation: u64,
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
                .spawn(move || {
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
                })
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

pub fn mercator_y(latitude: f64) -> f64 {
    (1.0 - latitude.clamp(-85.05112878, 85.05112878).to_radians().tan().asinh() / PI) / 2.0
}

/// Coverage for the application's north-up, unpitched camera. Projection of the
/// quads still uses MapLibre's own matrix, not an independent screen transform.
pub fn visible_tiles(parameters: &WgpuLayerParameters, source: &RasterSource) -> Vec<VisibleTile> {
    let metadata = source.metadata();
    let tile_size = f64::from(metadata.tile_size.unwrap_or(256));
    let z = (parameters.zoom + (512.0 / tile_size).log2())
        .ceil()
        .clamp(0.0, f64::from(metadata.max_zoom.clamp(0, 22))) as i32;
    let n = (1_u32 << z) as f64;
    let world = 512.0 * parameters.zoom.exp2();
    let center_x = (parameters.longitude + 180.0) / 360.0;
    let center_y = mercator_y(parameters.latitude);
    let left = ((center_x - parameters.width / (2.0 * world)) * n).floor() as i64;
    let right = ((center_x + parameters.width / (2.0 * world)) * n).ceil() as i64;
    let top = ((center_y - parameters.height / (2.0 * world)) * n).floor().max(0.0) as i32;
    let bottom = ((center_y + parameters.height / (2.0 * world)) * n).ceil().min(n) as i32;
    let [west, south, east, north] = metadata.bounds;
    let min_y = mercator_y(north) * n;
    let max_y = mercator_y(south) * n;
    let min_x = (west + 180.0) / 360.0 * n;
    let max_x = (east + 180.0) / 360.0 * n;
    let mut tiles = Vec::new();
    for y in top..bottom {
        if f64::from(y) >= max_y || f64::from(y + 1) <= min_y {
            continue;
        }
        for x in left..right {
            let canonical_x = x.rem_euclid(1 << z) as f64;
            let intersects = if west <= east {
                canonical_x < max_x && canonical_x + 1.0 > min_x
            } else {
                canonical_x < max_x || canonical_x + 1.0 > min_x
            };
            if intersects {
                tiles.push(VisibleTile { z, x, y });
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

pub fn parent(tile: Tile) -> Option<Tile> {
    (tile.z > 0).then(|| Tile {
        z: tile.z - 1,
        x: tile.x / 2,
        y: tile.y / 2,
    })
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
    use super::*;

    #[test]
    fn world_copies_share_a_numeric_tile_identity() {
        assert_eq!(VisibleTile { z: 3, x: -1, y: 2 }.canonical(), Tile { z: 3, x: 7, y: 2 });
        assert_eq!(VisibleTile { z: 3, x: 15, y: 2 }.canonical(), Tile { z: 3, x: 7, y: 2 });
    }

    #[test]
    fn parent_fallback_samples_only_the_child_footprint() {
        let tile = Tile { z: 3, x: 7, y: 5 };
        assert_eq!(parent_uv(tile, Tile { z: 1, x: 1, y: 1 }), [0.75, 0.25, 0.25, 0.25]);
        assert_eq!(parent_uv(tile, tile), [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(parent(Tile { z: 0, x: 0, y: 0 }), None);
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
