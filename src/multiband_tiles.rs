//! Bounded, generation-cancelled window reads and raw GPU texture arrays.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result, ensure};
use geo::Tile;

use crate::raster_source::{NumericTile, RasterSource};

pub(super) const MAX_JOBS: usize = 4;
pub(super) const MAX_CACHE_BYTES: usize = 256 * 1024 * 1024;
// Reserve a second grid for the displayed frame while its replacement loads.
pub(super) const MAX_GRID_BYTES: usize = MAX_CACHE_BYTES / 2;
pub(super) const MAX_CACHE_ENTRIES: usize = 256;
pub(super) const MAX_VISIBLE_TILES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct WindowKey {
    pub tile: Tile,
    pub start: usize,
}

struct WindowJob {
    generation: u64,
    key: WindowKey,
    source: Arc<RasterSource>,
    bands: Range<usize>,
}

pub(super) struct WindowResult {
    pub generation: u64,
    pub key: WindowKey,
    pub data: Result<Vec<Option<NumericTile>>>,
}

pub(super) struct WindowWorkers {
    jobs: async_channel::Sender<WindowJob>,
    results: async_channel::Receiver<WindowResult>,
    generation: Arc<AtomicU64>,
}

impl WindowWorkers {
    pub fn new() -> Result<Self> {
        let (jobs, receiver) = async_channel::bounded::<WindowJob>(MAX_JOBS);
        let (results, completed) = async_channel::bounded(MAX_JOBS);
        let generation = Arc::new(AtomicU64::new(0));
        for index in 0..2 {
            let receiver = receiver.clone();
            let results = results.clone();
            let generation = generation.clone();
            std::thread::Builder::new()
                .name(format!("multiband-tiles-{index}"))
                .spawn(move || run_worker(receiver, results, generation))
                .context("Could not start multiband tile workers")?;
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

    pub fn request(&self, generation: u64, key: WindowKey, source: Arc<RasterSource>, bands: Range<usize>) -> bool {
        self.jobs
            .try_send(WindowJob {
                generation,
                key,
                source,
                bands,
            })
            .is_ok()
    }

    pub fn completed(&self) -> Option<WindowResult> {
        self.results.try_recv().ok()
    }
}

impl Drop for WindowWorkers {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.jobs.close();
        self.results.close();
    }
}

fn run_worker(receiver: async_channel::Receiver<WindowJob>, results: async_channel::Sender<WindowResult>, generation: Arc<AtomicU64>) {
    while let Ok(job) = receiver.recv_blocking() {
        if job.generation != generation.load(Ordering::Relaxed) {
            continue;
        }
        let data = job
            .source
            .numeric_band_tiles_cancellable(job.key.tile, job.bands, || job.generation != generation.load(Ordering::Relaxed));
        if job.generation != generation.load(Ordering::Relaxed) {
            continue;
        }
        if results
            .send_blocking(WindowResult {
                generation: job.generation,
                key: job.key,
                data,
            })
            .is_err()
        {
            break;
        }
    }
}

pub(super) struct WindowTexture {
    pub values: wgpu::TextureView,
    pub mask: wgpu::TextureView,
    pub bytes: usize,
}

pub(super) struct CachedWindow {
    // None means authoritative empty coverage OR a reported permanent failure.
    // Both resolve the readiness barrier; neither is fetched again every paint.
    pub texture: Option<WindowTexture>,
    pub used: u64,
}

impl CachedWindow {
    pub fn bytes(&self) -> usize {
        self.texture.as_ref().map_or(0, |texture| texture.bytes)
    }
}

pub(super) fn upload_window(device: &wgpu::Device, queue: &wgpu::Queue, data: Vec<Option<NumericTile>>, size: u32) -> Result<Option<WindowTexture>> {
    validate_window(&data, size)?;
    if data.iter().all(Option::is_none) {
        return Ok(None);
    }
    let layers = data.len() as u32;
    let values = texture(device, size, layers, wgpu::TextureFormat::R32Float);
    let mask = texture(device, size, layers, wgpu::TextureFormat::R8Unorm);
    for (layer, tile) in data.into_iter().enumerate() {
        if let Some(tile) = tile {
            upload_band(queue, &values, &mask, layer as u32, tile);
        }
    }
    let view = wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2Array),
        ..Default::default()
    };
    Ok(Some(WindowTexture {
        values: values.create_view(&view),
        mask: mask.create_view(&view),
        bytes: size as usize * size as usize * layers as usize * 5,
    }))
}

fn validate_window(data: &[Option<NumericTile>], size: u32) -> Result<()> {
    ensure!(!data.is_empty(), "Multiband tile returned no band slots");
    for tile in data.iter().flatten() {
        ensure!(
            tile.width == size && tile.height == size,
            "Multiband tile dimensions differ from source tile size {size}"
        );
        ensure!(tile.values.len() == size as usize * size as usize, "Multiband tile has an invalid pixel count");
    }
    Ok(())
}

fn upload_band(queue: &wgpu::Queue, values: &wgpu::Texture, mask: &wgpu::Texture, layer: u32, mut tile: NumericTile) {
    // Explicit masks avoid WGSL's non-portable NaN handling. Keep valid values raw.
    let valid: Vec<u8> = tile.values.iter_mut().map(mask_value).collect();
    write_layer(queue, values, layer, bytemuck::cast_slice(&tile.values), tile.width, 4);
    write_layer(queue, mask, layer, &valid, tile.width, 1);
}

fn mask_value(value: &mut f32) -> u8 {
    if value.is_finite() {
        255
    } else {
        *value = 0.0;
        0
    }
}

pub(super) fn texture(device: &wgpu::Device, size: u32, layers: u32, format: wgpu::TextureFormat) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("Multiband raw data"),
        size: wgpu::Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: layers,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn write_layer(queue: &wgpu::Queue, texture: &wgpu::Texture, layer: u32, bytes: &[u8], size: u32, stride: u32) {
    let mut destination = texture.as_image_copy();
    destination.origin.z = layer;
    queue.write_texture(
        destination,
        bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size * stride),
            rows_per_image: Some(size),
        },
        wgpu::Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 1,
        },
    );
}

#[cfg(test)]
pub(super) fn cog_fixture(bands: usize) -> Result<(tempfile::TempDir, Vec<Arc<RasterSource>>)> {
    let directory = tempfile::tempdir()?;
    let input_path = directory.path().join("input.tif");
    crate::raster_source::test_support::write_raster(&input_path, 3857, bands)?;
    let path = directory.path().join("playback.tif");
    write_test_cog(&input_path, &path)?;
    let sources = RasterSource::open(&path)?;
    ensure!(sources.len() == bands, "COG fixture did not preserve band count");
    Ok((directory, sources))
}

#[cfg(test)]
fn write_test_cog(input_path: &std::path::Path, path: &std::path::Path) -> Result<()> {
    let input = gdal::Dataset::open(input_path)?;
    let options = gdal::raster::RasterCreationOptions::from_iter([
        "TILING_SCHEME=GoogleMapsCompatible",
        "ZOOM_LEVEL=3",
        "BLOCKSIZE=256",
        "OVERVIEWS=NONE",
        "COMPRESS=DEFLATE",
        "INTERLEAVE=BAND",
        "ADD_ALPHA=NO",
    ]);
    input.create_copy(&gdal::DriverManager::get_driver_by_name("COG")?, path, &options)?.close()?;
    // The native COG probe reads a complete 16-KiB header even for tiny fixtures.
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.set_len(file.metadata()?.len().max(16 * 1024))?;
    let metadata = geo::geotiff::GeoTiffMetadata::from_file(path)?;
    geo::cog::WebTilesReader::new(metadata).context("COG fixture is not readable by the native tile reader")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_for_result(workers: &WindowWorkers) -> Result<WindowResult> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(result) = workers.completed() {
                return Ok(result);
            }
            ensure!(std::time::Instant::now() < deadline, "Multiband worker did not complete before deadline");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn workers_load_actual_batch_slots_in_requested_band_order() -> Result<()> {
        let (_directory, sources) = cog_fixture(23)?;
        let source = &sources[0];
        let key = WindowKey {
            tile: Tile { z: 3, x: 3, y: 3 },
            start: 20,
        };
        let workers = WindowWorkers::new()?;
        assert!(workers.request(0, key, source.clone(), 21..24));
        let result = wait_for_result(&workers)?;
        assert_eq!((result.generation, result.key), (0, key));
        let data = result.data?;
        assert_eq!(data.len(), 3);
        for (index, tile) in data.into_iter().enumerate() {
            let tile = tile.context("Missing batch tile")?;
            let single = sources[20 + index].numeric_tile(key.tile, 1)?.context("Missing single tile")?;
            assert_eq!((tile.width, tile.height), (single.width, single.height));
            assert!(tile.values.iter().any(|value| value.is_finite()));
            assert!(tile.values.iter().zip(&single.values).all(|(a, b)| a == b || (a.is_nan() && b.is_nan())));
        }
        Ok(())
    }

    #[test]
    fn workers_discard_obsolete_generation_and_complete_replacement() -> Result<()> {
        let (_directory, sources) = cog_fixture(3)?;
        let workers = WindowWorkers::new()?;
        let generation = workers.reset();
        let key = WindowKey {
            tile: Tile { z: 3, x: 3, y: 3 },
            start: 0,
        };
        assert!(workers.request(generation - 1, key, sources[0].clone(), 1..4));
        assert!(workers.request(generation, key, sources[0].clone(), 1..4));
        let result = wait_for_result(&workers)?;
        assert_eq!((result.generation, result.data?.len()), (generation, 3));
        assert!(workers.completed().is_none());
        Ok(())
    }

    #[test]
    fn workers_return_invalid_band_failures_instead_of_losing_jobs() -> Result<()> {
        let (_directory, sources) = cog_fixture(3)?;
        let workers = WindowWorkers::new()?;
        let key = WindowKey {
            tile: Tile { z: 3, x: 3, y: 3 },
            start: 0,
        };
        assert!(workers.request(0, key, sources[0].clone(), 99..100));
        let result = wait_for_result(&workers)?;
        assert!(result.data.is_err());
        Ok(())
    }

    #[test]
    fn nodata_is_zeroed_and_masked_without_touching_valid_raw_values() {
        let mut values = [f32::NAN, f32::INFINITY, -12.5, 0.0];
        let mask: Vec<_> = values.iter_mut().map(mask_value).collect();
        assert_eq!((values, mask), ([0.0, 0.0, -12.5, 0.0], vec![0, 0, 255, 255]));
    }

    #[test]
    fn empty_coverage_is_a_valid_resolved_window() {
        assert!(validate_window(&[None, None], 256).is_ok());
    }

    #[test]
    fn malformed_tiles_fail_before_gpu_allocation() {
        let tile = NumericTile {
            width: 256,
            height: 256,
            values: vec![0.0],
        };
        assert!(validate_window(&[Some(tile)], 256).is_err());
    }
}
