use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock, Weak};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, bail, ensure};
use geo::Tile;
use inf::Legend;
use inf::colormap::{ColorMap, ColorMapDirection, ColorMapPreset};
use maplibre_native::file_source::{ErrorReason, FileSource, FileSourceType, RequestHandle, ResourceRequest, Responder, Response, register_file_source};
use serde_json::{Value, json};
use tiler::tileproviderfactory::TileProviderOptions;
use tiler::{ColorMappedTileRequest, DynamicTileProvider, LayerMetadata, LayerSourceType, TileFormat, TileProvider, WarpingTileProvider};

const TILE_SIZE: u32 = 256;
const URL_PREFIX: &str = "raster://";
static NEXT_STYLE_ID: AtomicU64 = AtomicU64::new(1);

/// An immutable tile style. Its URL identity changes with the color mapping so
/// MapLibre never reuses tiles from a previous range or palette.
pub struct RasterSource {
    id: u64,
    metadata: Arc<LayerMetadata>,
    legend: Legend,
    palette: ColorMapPreset,
    range: (f32, f32),
    errors: async_channel::Sender<String>,
    error_receiver: async_channel::Receiver<String>,
}

impl RasterSource {
    pub fn open(path: &Path) -> Result<Vec<Arc<Self>>> {
        let options = TileProviderOptions {
            calculate_stats: false,
            zoom_level_strategy: geo::ZoomLevelStrategy::PreferHigher,
        };
        let mut provider = DynamicTileProvider::new(options.clone());
        // The optimized COG reader only accepts web-tile-aligned COGs. Other
        // COGs and GDAL formats (including VRT/ASCII) use on-demand warping.
        let mut metadata = provider
            .add_file(path)
            .or_else(|_| WarpingTileProvider::new(path, &options).map(|provider| provider.layers()))
            .with_context(|| format!("Could not open {}", path.display()))?;
        if metadata.iter().any(|meta| meta.source_format == LayerSourceType::CloudOptimizedGeoTiff) && gdal::Dataset::open(path)?.raster_count() > 1 {
            metadata = WarpingTileProvider::new(path, &options)?.layers();
            for meta in &mut metadata {
                meta.source_format = LayerSourceType::GeoTiff;
            }
        }
        ensure!(!metadata.is_empty(), "No raster bands found in {}", path.display());
        metadata
            .into_iter()
            .map(|mut metadata| {
                if metadata.source_format == LayerSourceType::Unknown {
                    // Metadata for VRT and other GDAL-only drivers has no tiler
                    // source tag; route it through the generic warping provider.
                    metadata.source_format = LayerSourceType::GeoTiff;
                }
                ensure!(
                    metadata.bounds.iter().all(|value| value.is_finite()),
                    "The raster has invalid geographic bounds"
                );
                if metadata.source_format != LayerSourceType::CloudOptimizedGeoTiff && metadata.band_nr.is_some() {
                    // GDAL can synthesize tiles below the stored overview levels.
                    metadata.min_zoom = 0;
                    metadata.max_zoom = metadata.max_zoom.max(0);
                }
                if metadata.source_format == LayerSourceType::Mbtiles {
                    ensure!(
                        matches!(metadata.tile_format, TileFormat::Png | TileFormat::Jpeg),
                        "Only PNG/JPEG raster MBTiles are supported"
                    );
                }
                // Unlike the POC's debug-mode statistics, force calculation when a
                // file has no stored statistics. Otherwise its range is 0..f32::MAX.
                if metadata.band_nr.is_some() {
                    let dataset = gdal::Dataset::open(&metadata.path)?;
                    if let Some(stats) = dataset.rasterband(metadata.band_nr.unwrap_or(1))?.get_statistics(true, true)? {
                        metadata.min_value = stats.min as f32;
                        metadata.max_value = stats.max as f32;
                    }
                }
                let range = display_range(metadata.min_value, metadata.max_value);
                Self::styled(Arc::new(metadata), ColorMapPreset::Terrain, range)
            })
            .collect()
    }

    fn styled(metadata: Arc<LayerMetadata>, palette: ColorMapPreset, range: (f32, f32)) -> Result<Arc<Self>> {
        ensure!(
            range.0.is_finite() && range.1.is_finite() && (range.1 - range.0).is_finite() && range.0 < range.1,
            "Minimum must be less than maximum; both must be finite numbers."
        );
        let legend = Legend::linear(&ColorMap::Preset(palette, ColorMapDirection::Regular), range.0..range.1, None)?;
        let (errors, error_receiver) = async_channel::bounded(1);
        Ok(Arc::new(Self {
            id: NEXT_STYLE_ID.fetch_add(1, Ordering::Relaxed),
            metadata,
            legend,
            palette,
            range,
            errors,
            error_receiver,
        }))
    }

    pub fn restyled(&self, palette: ColorMapPreset, range: (f32, f32)) -> Result<Arc<Self>> {
        Self::styled(self.metadata.clone(), palette, range)
    }

    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn metadata(&self) -> &LayerMetadata {
        &self.metadata
    }
    pub fn range(&self) -> (f32, f32) {
        self.range
    }
    pub fn data_range(&self) -> (f32, f32) {
        display_range(self.metadata.min_value, self.metadata.max_value)
    }
    pub fn palette(&self) -> ColorMapPreset {
        self.palette
    }
    pub fn legend(&self) -> &Legend {
        &self.legend
    }

    pub fn errors(&self) -> async_channel::Receiver<String> {
        self.error_receiver.clone()
    }

    pub fn source_json(&self, dpi: u8) -> Value {
        let dpi = if self.metadata.supports_dpi_ratio { dpi.clamp(1, 4) } else { 1 };
        json!({
            "type": "raster",
            "tiles": [format!("{URL_PREFIX}{}/{{z}}/{{x}}/{{y}}@{dpi}.png", self.id)],
            "tileSize": self.metadata.tile_size.unwrap_or(TILE_SIZE),
            "scheme": self.metadata.scheme,
            "minzoom": if self.metadata.band_nr.is_some() { 0 } else { self.metadata.min_zoom.clamp(0, 22) },
            "maxzoom": self.metadata.max_zoom.clamp(0, 22),
            "bounds": [self.metadata.bounds[0], self.metadata.bounds[1].clamp(-85.05112878, 85.05112878), self.metadata.bounds[2], self.metadata.bounds[3].clamp(-85.05112878, 85.05112878)]
        })
    }

    pub fn layer_json(&self, opacity: f32) -> Value {
        json!({
            "id": "local-raster-layer", "type": "raster", "source": "local-raster",
            "paint": { "raster-opacity": opacity, "raster-resampling": "nearest", "raster-fade-duration": 0 }
        })
    }

    fn tile(&self, address: TileAddress) -> Result<Response> {
        let metadata = &self.metadata;
        // MBTiles can contain already-rendered PNG/JPEG tiles as well as numeric
        // raster tiles. The tiler passes image tiles through unchanged.
        let request = ColorMappedTileRequest {
            tile: address.tile,
            dpi_ratio: address.dpi,
            tile_size: metadata.tile_size.unwrap_or(TILE_SIZE),
            legend: &self.legend,
        };
        let warp = || {
            let mut metadata = (**metadata).clone();
            metadata.min_zoom = 0;
            WarpingTileProvider::color_mapped_tile(&metadata, &request)
        };
        let data = if metadata.source_format == LayerSourceType::CloudOptimizedGeoTiff {
            // An overview pyramid need not reach zoom zero. Synthesize coarser
            // tiles so fitting the full raster never makes it disappear.
            if address.tile.z < metadata.min_zoom {
                warp()?
            } else {
                DynamicTileProvider::get_tile_color_mapped_for_layer(metadata, &request).or_else(|_| warp())?
            }
        } else {
            DynamicTileProvider::get_tile_color_mapped_for_layer(metadata, &request)?
        };
        ensure!(
            matches!(data.format, TileFormat::Png | TileFormat::Jpeg) || data.data.is_empty(),
            "The raster provider returned a non-image tile"
        );
        Ok(if data.data.is_empty() {
            Response::no_content()
        } else {
            Response::data(data.data).with_expires(SystemTime::now() + Duration::from_secs(3600))
        })
    }
}

fn display_range(min: f32, max: f32) -> (f32, f32) {
    if min.is_finite() && max.is_finite() && (max - min).is_finite() && min < max {
        (min, max)
    } else if min.is_finite() && min == max {
        let padding = (min.abs() * 0.01).max(1.0);
        ((min - padding).max(f32::MIN), (max + padding).min(f32::MAX))
    } else {
        (0.0, 255.0)
    }
}

#[derive(Debug)]
struct TileAddress {
    source: u64,
    tile: Tile,
    dpi: u8,
}

impl TileAddress {
    fn parse(url: &str) -> Result<Self> {
        let Some(path) = url.strip_prefix(URL_PREFIX).and_then(|path| path.strip_suffix(".png")) else {
            bail!("Invalid raster tile URL")
        };
        let parts: Vec<_> = path.split('/').collect();
        ensure!(parts.len() == 4, "Invalid raster tile path");
        let (y, dpi) = parts[3].split_once('@').context("Missing raster tile DPI")?;
        let source = parts[0].parse()?;
        let z: i32 = parts[1].parse()?;
        ensure!((0..=22).contains(&z), "Invalid raster tile zoom");
        let tile = Tile {
            z,
            x: parts[2].parse()?,
            y: y.parse()?,
        };
        ensure!(
            (0..(1 << z)).contains(&tile.x) && (0..(1 << z)).contains(&tile.y),
            "Invalid raster tile coordinate"
        );
        let dpi = dpi.parse()?;
        ensure!((1..=4).contains(&dpi), "Invalid raster tile DPI");
        Ok(Self { source, tile, dpi })
    }
}

type Sources = Arc<RwLock<HashMap<u64, Weak<RasterSource>>>>;
struct TileJob {
    url: String,
    responder: Responder,
    cancelled: Arc<AtomicBool>,
}

/// Process-wide protocol adapter. Only raster:// requests are intercepted;
/// MapLibre's normal network source still handles the vector basemap.
pub struct RasterTiles {
    sources: Sources,
}

impl RasterTiles {
    pub fn init() -> Result<&'static Self> {
        static SERVICE: OnceLock<Result<RasterTiles, String>> = OnceLock::new();
        SERVICE
            .get_or_init(|| Self::start().map_err(|error| format!("{error:#}")))
            .as_ref()
            .map_err(|error| anyhow::anyhow!(error.clone()))
    }

    fn start() -> Result<Self> {
        let sources: Sources = Arc::default();
        let (jobs, receiver) = async_channel::bounded::<TileJob>(128);
        // Bound expensive GDAL work, and skip cancelled queued jobs when the
        // camera moves. GDAL datasets are opened per tile, never shared.
        for worker in 0..2 {
            let receiver = receiver.clone();
            let sources = sources.clone();
            std::thread::Builder::new().name(format!("raster-tiles-{worker}")).spawn(move || {
                while let Ok(job) = receiver.recv_blocking() {
                    if job.cancelled.load(Ordering::Relaxed) {
                        continue;
                    }
                    let response = tile_response(&sources, &job.url);
                    job.responder.complete(response);
                }
            })?;
        }
        register_file_source(FileSourceType::Asset, RasterFileSource { jobs });
        Ok(Self { sources })
    }

    pub fn register(&self, source: &Arc<RasterSource>) -> Result<()> {
        let mut sources = self.sources.write().map_err(|_| anyhow::anyhow!("Raster registry lock poisoned"))?;
        sources.retain(|_, source| source.strong_count() > 0);
        sources.insert(source.id, Arc::downgrade(source));
        Ok(())
    }
}

fn tile_response(sources: &Sources, url: &str) -> Response {
    let result = (|| -> Result<Response> {
        let address = TileAddress::parse(url)?;
        let source = sources
            .read()
            .map_err(|_| anyhow::anyhow!("Raster registry lock poisoned"))?
            .get(&address.source)
            .and_then(Weak::upgrade);
        match source {
            Some(source) => source.tile(address).inspect_err(|error| {
                let _ = source.errors.try_send(format!("Couldn’t render raster tiles: {error:#}"));
            }),
            None => Ok(Response::no_content()),
        }
    })();
    result.unwrap_or_else(|error| {
        log::error!("Raster tile {url} failed: {error:#}");
        Response::error(ErrorReason::Other, format!("{error:#}"))
    })
}

struct RasterFileSource {
    jobs: async_channel::Sender<TileJob>,
}

impl FileSource for RasterFileSource {
    fn can_request(&self, request: &ResourceRequest) -> bool {
        request.url.starts_with(URL_PREFIX)
    }

    fn request(&self, request: ResourceRequest, responder: Responder) -> RequestHandle {
        let cancelled = Arc::new(AtomicBool::new(false));
        if let Err(error) = self.jobs.try_send(TileJob {
            url: request.url,
            responder,
            cancelled: cancelled.clone(),
        }) {
            error
                .into_inner()
                .responder
                .complete(Response::error(ErrorReason::Other, "Raster tile queue is full or stopped"));
            return RequestHandle::Done;
        }
        RequestHandle::pending(move || {
            cancelled.store(true, Ordering::Relaxed);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_urls_validate_coordinates_and_dpi() {
        let address = TileAddress::parse("raster://42/9/264/171@2.png").unwrap();
        assert_eq!(
            (address.source, address.tile.z, address.tile.x, address.tile.y, address.dpi),
            (42, 9, 264, 171, 2)
        );
        for url in [
            "file:///etc/passwd",
            "raster://1/23/0/0@1.png",
            "raster://1/0/1/0@1.png",
            "raster://1/1/-1/0@1.png",
            "raster://1/0/0/0@0.png",
            "raster://1/0/0/0@9.png",
            "raster://1/0/0/0.png",
            "raster://1/0/0/0@1.png/extra",
        ] {
            assert!(TileAddress::parse(url).is_err(), "Accepted {url}");
        }
    }

    #[test]
    fn constant_and_missing_statistics_have_usable_ranges() {
        assert_eq!(display_range(5.0, 5.0), (4.0, 6.0));
        assert_eq!(display_range(f32::NAN, f32::NAN), (0.0, 255.0));
    }

    #[test]
    fn geographic_rasters_are_warped_into_transparent_png_tiles() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("geographic.tif");
        super::test_support::write_raster(&path, 4326, 2)?;
        let sources = RasterSource::open(&path)?;
        assert_eq!(sources.len(), 2, "Each numeric band must be selectable");
        let source = &sources[0];
        assert!(source.range().0 < source.range().1);
        assert!(source.range().0 >= 0.0, "Nodata must not affect the color range");
        let response = source.tile(TileAddress::parse(&format!("raster://{}/3/3/3@2.png", source.id()))?)?;
        let image = image::load_from_memory(response.data.as_ref().context("Missing PNG tile")?)?.to_rgba8();
        assert_eq!(image.dimensions(), (512, 512));
        assert!(image.pixels().any(|pixel| pixel[3] == 0), "Outside pixels and nodata must be transparent");
        assert!(image.pixels().any(|pixel| pixel[3] == 255), "The warped tile must contain raster data");
        let empty = source.tile(TileAddress::parse(&format!("raster://{}/4/0/0@1.png", source.id()))?)?;
        assert!(empty.no_content, "Tiles outside the raster must be empty, not failed PNGs");
        Ok(())
    }

    #[test]
    fn palette_and_range_changes_invalidate_tile_identity_and_pixels() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("mercator.tif");
        super::test_support::write_raster(&path, 3857, 1)?;
        let source = RasterSource::open(&path)?.remove(0);
        let gray = source.restyled(ColorMapPreset::Gray, (0.0, 1000.0))?;
        assert_ne!(source.id(), gray.id());
        let address = || TileAddress {
            source: source.id(),
            tile: Tile { z: 3, x: 3, y: 3 },
            dpi: 1,
        };
        assert_ne!(source.tile(address())?.data, gray.tile(address())?.data);
        assert!(source.restyled(ColorMapPreset::Gray, (5.0, 5.0)).is_err());
        assert!(source.restyled(ColorMapPreset::Gray, (f32::NAN, 5.0)).is_err());
        assert_eq!(gray.source_json(2)["tiles"][0], format!("raster://{}/{{z}}/{{x}}/{{y}}@2.png", gray.id()));
        Ok(())
    }

    #[test]
    fn protocol_returns_no_content_for_released_sources_and_errors_for_invalid_urls() -> Result<()> {
        let sources: Sources = Arc::default();
        assert!(tile_response(&sources, "raster://999/0/0/0@1.png").no_content);
        assert!(tile_response(&sources, "raster://999/0/0/0@0.png").error.is_some());
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("released.tif");
        super::test_support::write_raster(&path, 4326, 1)?;
        let source = RasterSource::open(&path)?.remove(0);
        let url = format!("raster://{}/3/3/3@1.png", source.id());
        sources.write().unwrap().insert(source.id(), Arc::downgrade(&source));
        assert!(tile_response(&sources, &url).data.is_some());
        drop(source);
        assert!(tile_response(&sources, &url).no_content);
        Ok(())
    }

    #[test]
    fn missing_and_non_raster_inputs_return_errors() -> Result<()> {
        let dir = tempfile::tempdir()?;
        assert!(RasterSource::open(&dir.path().join("missing.tif")).is_err());
        let path = dir.path().join("not-a-raster.txt");
        std::fs::write(&path, "not raster data")?;
        assert!(RasterSource::open(&path).is_err());
        Ok(())
    }

    #[test]
    fn optimized_cogs_use_native_tiles_and_synthesize_missing_overviews() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("input.tif");
        super::test_support::write_raster(&path, 3857, 1)?;
        let cog_path = dir.path().join("aligned-cog.tif");
        let input = gdal::Dataset::open(&path)?;
        let options = gdal::raster::RasterCreationOptions::from_iter([
            "TILING_SCHEME=GoogleMapsCompatible",
            "ZOOM_LEVEL=3",
            "BLOCKSIZE=256",
            "OVERVIEWS=NONE",
            "COMPRESS=DEFLATE",
            "ADD_ALPHA=NO",
        ]);
        let cog = input.create_copy(&gdal::DriverManager::get_driver_by_name("COG")?, &cog_path, &options)?;
        drop(cog);
        // The tiler's COG probe reads a full 16-KiB header. Keep this tiny,
        // highly compressible fixture large enough to exercise that path.
        let file = std::fs::OpenOptions::new().write(true).open(&cog_path)?;
        file.set_len(file.metadata()?.len().max(16 * 1024))?;
        let source = RasterSource::open(&cog_path)?.remove(0);
        assert_eq!(source.metadata().source_format, LayerSourceType::CloudOptimizedGeoTiff);
        assert_eq!(source.source_json(2)["minzoom"], 0);
        assert!(
            source.source_json(2)["tiles"][0].as_str().unwrap().ends_with("@1.png"),
            "Optimized COG tiles must not request unsupported DPI ratios"
        );
        for (z, x, y) in [(3, 3, 3), (2, 1, 1)] {
            let tile = source.tile(TileAddress {
                source: source.id(),
                tile: Tile { z, x, y },
                dpi: 1,
            })?;
            let image = image::load_from_memory(tile.data.as_ref().context("Missing COG tile")?)?.to_rgba8();
            assert_eq!(image.dimensions(), (256, 256));
            assert!(image.pixels().any(|pixel| pixel[3] == 255));
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub fn write_raster(path: &Path, epsg: u32, bands: usize) -> Result<()> {
        let mut dataset = gdal::DriverManager::get_driver_by_name("GTiff")?.create_with_band_type::<f32, _>(path, 64, 64, bands)?;
        let extent = if epsg == 4326 { 10.0 } else { 1_000_000.0 };
        dataset.set_geo_transform(&[-extent, extent / 32.0, 0.0, extent, 0.0, -extent / 32.0])?;
        dataset.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(epsg)?)?;
        for band_nr in 1..=bands {
            let mut band = dataset.rasterband(band_nr)?;
            band.set_no_data_value(Some(-9999.0))?;
            let values = (0..4096)
                .map(|ix| if ix % 64 < 8 { -9999.0 } else { (ix % 64 + ix / 64 + band_nr * 10) as f32 })
                .collect();
            band.write((0, 0), (64, 64), &mut gdal::raster::Buffer::new((64, 64), values))?;
        }
        dataset.flush_cache()?;
        Ok(())
    }
}
