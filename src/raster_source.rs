use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock, Weak};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, bail, ensure};
use geo::{AnyDenseArray, Array, Tile};
use inf::Legend;
use inf::colormap::{ColorMap, ColorMapDirection, ColorMapPreset, ProcessedColorMap};
use maplibre_native::file_source::{ErrorReason, FileSource, FileSourceType, RequestHandle, ResourceRequest, Responder, Response, register_file_source};
use raster_tile::RasterTileIO;
use serde_json::{Value, json};
use tiler::tileproviderfactory::TileProviderOptions;
use tiler::{DynamicTileProvider, LayerMetadata, LayerSourceType, TileFormat, TileProvider, TileRequest, WarpingTileProvider};

const TILE_SIZE: u32 = 256;
const URL_PREFIX: &str = "raster://";
static NEXT_STYLE_ID: AtomicU64 = AtomicU64::new(1);

/// An immutable display-style snapshot. Numeric tile caches use metadata.id,
/// not this snapshot's ID, so styling never invalidates the data.
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

    pub fn is_numeric(&self) -> bool {
        self.metadata.band_nr.is_some()
    }

    pub fn report_tile_error(&self, error: &anyhow::Error) {
        let _ = self.errors.try_send(format!("Couldn’t render raster tiles: {error:#}"));
    }

    /// Raw data in a GPU-friendly format, independent of range and palette.
    /// NaN represents nodata; numeric values are never normalized or clipped.
    pub fn numeric_tile(&self, tile: Tile, dpi: u8) -> Result<Option<NumericTile>> {
        let metadata = &self.metadata;
        let request = TileRequest {
            tile,
            dpi_ratio: dpi,
            tile_size: metadata.tile_size.unwrap_or(TILE_SIZE),
            tile_format: TileFormat::RasterTile,
        };
        let warp = || {
            let mut metadata = (**metadata).clone();
            metadata.min_zoom = 0;
            WarpingTileProvider::tile(&metadata, &request)
        };
        let raw = if metadata.source_format == LayerSourceType::CloudOptimizedGeoTiff {
            if tile.z < metadata.min_zoom {
                warp()?
            } else {
                DynamicTileProvider::get_tile_for_layer(metadata, &request).or_else(|_| warp())?
            }
        } else {
            DynamicTileProvider::get_tile_for_layer(metadata, &request)?
        };
        if raw.data.is_empty() {
            return Ok(None);
        }
        let data: AnyDenseArray = AnyDenseArray::from_raster_tile_bytes(&raw.data)?;
        let data = data.cast_to::<f32>();
        let width = data.columns().0 as u32;
        let height = data.rows().0 as u32;
        let values = data.iter_opt().map(|value| value.filter(|v| v.is_finite()).unwrap_or(f32::NAN)).collect();
        Ok(Some(NumericTile { width, height, values }))
    }

    pub fn palette_rgba(&self) -> Vec<u8> {
        // Sample the same processed palette as the legend, without involving
        // the display range (which might have tiny or very large values).
        let palette = ProcessedColorMap::<256>::create_for_preset(self.palette, ColorMapDirection::Regular);
        (0..=255)
            .flat_map(|ix| {
                let color = palette.get_color_by_value(ix);
                [color.r, color.g, color.b, color.a]
            })
            .collect()
    }

    pub fn source_json(&self, dpi: u8) -> Value {
        let dpi = if self.metadata.supports_dpi_ratio { dpi.clamp(1, 4) } else { 1 };
        json!({
            "type": "raster",
            "tiles": [format!("{URL_PREFIX}{}/{{z}}/{{x}}/{{y}}@{dpi}.png", self.metadata.id)],
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
        ensure!(!self.is_numeric(), "Numeric rasters are rendered by the custom layer");
        // This protocol serves image-only MBTiles. Numeric data never goes
        // through PNG encoding or MapLibre's image decoder.
        let request = TileRequest {
            tile: address.tile,
            dpi_ratio: address.dpi,
            tile_size: metadata.tile_size.unwrap_or(TILE_SIZE),
            tile_format: metadata.tile_format,
        };
        let data = DynamicTileProvider::get_tile_for_layer(metadata, &request)?;
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

#[non_exhaustive]
pub struct NumericTile {
    pub width: u32,
    pub height: u32,
    pub values: Vec<f32>,
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

type Sources = Arc<RwLock<HashMap<tiler::LayerId, Weak<RasterSource>>>>;
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
        sources.insert(source.metadata.id, Arc::downgrade(source));
        Ok(())
    }
}

fn tile_response(sources: &Sources, url: &str) -> Response {
    let result = (|| -> Result<Response> {
        let address = TileAddress::parse(url)?;
        let source = sources
            .read()
            .map_err(|_| anyhow::anyhow!("Raster registry lock poisoned"))?
            .get(&address.source.into())
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
    fn geographic_rasters_are_warped_into_raw_numeric_tiles_with_nodata() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("geographic.tif");
        super::test_support::write_raster(&path, 4326, 2)?;
        let sources = RasterSource::open(&path)?;
        assert_eq!(sources.len(), 2, "Each numeric band must be selectable");
        let source = &sources[0];
        assert!(source.range().0 < source.range().1);
        assert!(source.range().0 >= 0.0, "Nodata must not affect the color range");
        let tile = source.numeric_tile(Tile { z: 3, x: 3, y: 3 }, 2)?.context("Missing numeric tile")?;
        assert_eq!((tile.width, tile.height), (512, 512));
        assert!(tile.values.iter().any(|value| value.is_nan()), "Outside pixels and nodata must stay invalid");
        assert!(tile.values.iter().any(|value| value.is_finite()), "The warped tile must contain raster data");
        let empty = source.numeric_tile(Tile { z: 4, x: 0, y: 0 }, 1)?;
        assert!(empty.is_none(), "Tiles outside the raster must be empty");
        Ok(())
    }

    #[test]
    fn mercator_tiles_keep_declared_nodata_invalid() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("nodata.tif");
        super::test_support::write_raster(&path, 3857, 1)?;
        let source = RasterSource::open(&path)?.remove(0);
        let tile = source.numeric_tile(Tile { z: 3, x: 3, y: 3 }, 1)?.context("Missing numeric tile")?;
        assert!(tile.values[0].is_nan(), "Padding must stay invalid");
        assert!(
            tile.values[250 * tile.width as usize + 210].is_nan(),
            "Declared nodata inside the raster must stay invalid"
        );
        assert!(tile.values[250 * tile.width as usize + 250].is_finite(), "Data must remain valid");
        Ok(())
    }

    #[test]
    fn mercator_tiles_without_declared_nodata_keep_padding_invalid_and_zero_valid() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("no-nodata.tif");
        let mut dataset = gdal::DriverManager::get_driver_by_name("GTiff")?.create_with_band_type::<f32, _>(&path, 64, 64, 1)?;
        dataset.set_geo_transform(&[-1_000_000.0, 31_250.0, 0.0, 1_000_000.0, 0.0, -31_250.0])?;
        dataset.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(3857)?)?;
        dataset.rasterband(1)?.fill(0.0, None)?;
        dataset.flush_cache()?;
        drop(dataset);

        let source = RasterSource::open(&path)?.remove(0);
        for (z, x, y) in [(3, 3, 3), (2, 1, 1)] {
            let tile = source.numeric_tile(Tile { z, x, y }, 1)?.context("Missing numeric tile")?;
            assert!(tile.values[0].is_nan(), "Padding outside the raster must stay invalid at zoom {z}");
            assert_eq!(
                tile.values[250 * tile.width as usize + 250],
                0.0,
                "Valid zero pixels must remain valid at zoom {z}"
            );
        }
        if let Some(tile) = source.numeric_tile(Tile { z: 3, x: 0, y: 0 }, 1)? {
            assert!(
                tile.values.iter().all(|value| value.is_nan()),
                "Tiles outside the raster must not contain valid padding"
            );
        }
        Ok(())
    }

    #[test]
    fn palette_and_range_changes_preserve_numeric_tile_identity_and_values() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("mercator.tif");
        super::test_support::write_raster(&path, 3857, 1)?;
        let source = RasterSource::open(&path)?.remove(0);
        let gray = source.restyled(ColorMapPreset::Gray, (0.0, 1000.0))?;
        assert_ne!(source.id(), gray.id());
        assert_eq!(source.metadata().id, gray.metadata().id, "Style snapshots share one data identity");
        let bits = |source: &RasterSource| -> Result<Vec<u32>> {
            Ok(source
                .numeric_tile(Tile { z: 3, x: 3, y: 3 }, 1)?
                .context("Missing tile")?
                .values
                .iter()
                .map(|v| v.to_bits())
                .collect())
        };
        assert_eq!(bits(&source)?, bits(&gray)?, "Raw values must not depend on the legend");
        assert_ne!(source.palette_rgba(), gray.palette_rgba());
        assert!(source.restyled(ColorMapPreset::Gray, (5.0, 5.0)).is_err());
        assert!(source.restyled(ColorMapPreset::Gray, (f32::NAN, 5.0)).is_err());
        assert_eq!(
            source.source_json(2),
            gray.source_json(2),
            "Image tile identities must also be style-independent"
        );
        Ok(())
    }

    #[test]
    fn protocol_returns_no_content_for_released_sources_and_rejects_numeric_tiles() -> Result<()> {
        let sources: Sources = Arc::default();
        assert!(tile_response(&sources, "raster://999/0/0/0@1.png").no_content);
        assert!(tile_response(&sources, "raster://999/0/0/0@0.png").error.is_some());
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("released.tif");
        super::test_support::write_raster(&path, 4326, 1)?;
        let source = RasterSource::open(&path)?.remove(0);
        let url = format!("raster://{}/3/3/3@1.png", source.metadata().id);
        sources.write().unwrap().insert(source.metadata().id, Arc::downgrade(&source));
        assert!(tile_response(&sources, &url).error.is_some(), "Numeric tiles must bypass the image protocol");
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
    fn byte_values_and_palette_entries_stay_valid_across_the_full_data_range() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("byte-values.tif");
        super::test_support::write_byte_raster(&path)?;
        let source = RasterSource::open(&path)?.remove(0);
        let tile = source.numeric_tile(Tile { z: 0, x: 0, y: 0 }, 1)?.context("Missing byte tile")?;
        assert_eq!((tile.width, tile.height), (256, 256));
        let palette = source.palette_rgba();
        assert!(palette.chunks_exact(4).all(|color| color[3] == 255));
        for value in 0..255 {
            assert_eq!(tile.values[128 * 256 + value], value as f32, "Valid byte {value} must not become nodata");
        }
        assert!(tile.values[128 * 256 + 255].is_nan(), "Only the nodata byte should be invalid");
        Ok(())
    }

    #[test]
    fn image_mbtiles_pass_through_unchanged_across_style_snapshots() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("image.mbtiles");
        // Build MBTiles directly: the static GDAL build need not enable its
        // optional MBTiles driver for our native SQLite tile provider to work.
        let db = sqlite::Connection::new(&path, sqlite::AccessMode::Create)?;
        db.execute_sql_statements(
            "CREATE TABLE metadata (name TEXT, value TEXT);
            INSERT INTO metadata VALUES ('format','png'), ('bounds','-180,-85,180,85'), ('minzoom','0'), ('maxzoom','0');
            CREATE TABLE tiles (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_data BLOB);",
        )?;
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(256, 256, image::Rgba([255, 0, 0, 255])).write_to(&mut png, image::ImageFormat::Png)?;
        let mut insert = db.prepare_statement("INSERT INTO tiles VALUES (0,0,0,?1) RETURNING zoom_level")?;
        insert.bind_blob(1, png.get_ref())?;
        ensure!(insert.next().is_some(), "Could not create fixture tile: {}", db.last_error());
        drop(insert);
        drop(db);
        let source = RasterSource::open(&path)?.remove(0);
        assert!(!source.is_numeric());
        let tiles = RasterTiles::init()?;
        tiles.register(&source)?;
        let url = format!("raster://{}/0/0/0@1.png", source.metadata().id);
        let before = tile_response(&tiles.sources, &url);
        let image = image::load_from_memory(before.data.as_ref().context("Missing image MBTile")?)?.to_rgba8();
        assert_eq!(*image.get_pixel(128, 128), image::Rgba([255, 0, 0, 255]));
        let gray = source.restyled(ColorMapPreset::Gray, (10.0, 100.0))?;
        tiles.register(&gray)?;
        assert_eq!(source.source_json(1), gray.source_json(1));
        drop(source);
        assert_eq!(
            tile_response(&tiles.sources, &url).data,
            before.data,
            "Palette/range edits must not recolor MBTiles or release their URL identity"
        );
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
            let tile = source.numeric_tile(Tile { z, x, y }, 1)?.context("Missing COG tile")?;
            assert_eq!((tile.width, tile.height), (256, 256));
            assert!(tile.values.iter().any(|value| value.is_finite()));
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub fn write_byte_raster(path: &Path) -> Result<()> {
        let mut dataset = gdal::DriverManager::get_driver_by_name("GTiff")?.create_with_band_type::<u8, _>(path, 256, 256, 1)?;
        let extent = 20_037_508.342_789_244;
        dataset.set_geo_transform(&[-extent, extent / 128.0, 0.0, extent, 0.0, -extent / 128.0])?;
        dataset.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(3857)?)?;
        let mut band = dataset.rasterband(1)?;
        band.set_no_data_value(Some(255.0))?;
        let values = (0..256 * 256).map(|ix| (ix % 256) as u8).collect();
        band.write((0, 0), (256, 256), &mut gdal::raster::Buffer::new((256, 256), values))?;
        dataset.flush_cache()?;
        Ok(())
    }

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
