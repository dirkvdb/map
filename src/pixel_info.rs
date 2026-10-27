use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use geo::{AnyDenseArray, Coordinate, GeoReference, Tile, crs};
use gpui_kit::component::ActiveTheme;
use gpui_kit::{
    AppContext, Context, InteractiveElement, IntoElement, ParentElement, Render, StatefulInteractiveElement, Styled, Task, TestSupportExt, Window, div,
};
use raster_tile::RasterTileIO;
use tiler::{DynamicTileProvider, LayerId, LayerSourceType, TileFormat, TileRequest, WarpingTileProvider};

use crate::raster_source::RasterSource;

const MAX_CACHED_TILES: usize = 16;
const MAX_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// Numeric tiles stay on the UI side for immediate, memory-only lookups while
/// other tiles load in the background. Palette/range changes do not invalidate them.
#[derive(Default)]
struct PixelValueCache {
    tiles: VecDeque<CachedTile>,
    bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TileKey {
    layer: LayerId,
    tile: Tile,
    dpi: u8,
}

struct CachedTile {
    key: TileKey,
    reference: GeoReference,
    data: Result<Option<AnyDenseArray>, String>,
}

impl CachedTile {
    fn load(source: &RasterSource, key: TileKey) -> Self {
        let start = Instant::now();
        let metadata = source.metadata();
        let tile_size = metadata.tile_size.unwrap_or(Tile::TILE_SIZE);
        let request = TileRequest {
            tile: key.tile,
            dpi_ratio: key.dpi,
            tile_size,
            tile_format: TileFormat::RasterTile,
        };
        let data = Self::read(source, &request).map_err(|error| {
            log::error!("Couldn’t read the raster pixel tile: {error:#}");
            format!("{error:#}")
        });
        log::debug!("Pixel tile {key:?} loaded in {:?}", start.elapsed());
        let reference = data
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .map(|data| Self::tile_reference(key.tile, data))
            .unwrap_or_default();
        Self { key, reference, data }
    }

    fn tile_reference(tile: Tile, data: &AnyDenseArray) -> GeoReference {
        let bounds = tile.web_mercator_bounds();
        // Derive cell sizes directly: zoom-based helpers assume power-of-two
        // resolutions and misregister DPI 3/4 samples and larger COG tiles.
        let cell_size = geo::CellSize::new(bounds.width() / f64::from(data.columns().0), -bounds.height() / f64::from(data.rows().0));
        GeoReference::with_top_left_origin(
            "EPSG:3857",
            geo::RasterSize::with_rows_cols(data.rows(), data.columns()),
            bounds.top_left(),
            cell_size,
            Some(f64::NAN),
        )
    }

    fn read(source: &RasterSource, request: &TileRequest) -> Result<Option<AnyDenseArray>> {
        let metadata = source.metadata();
        // Use the same native COG reader/GDAL fallback as the map tiles.
        let raw = DynamicTileProvider::get_tile_for_layer(metadata, request).or_else(|error| {
            if metadata.source_format == LayerSourceType::CloudOptimizedGeoTiff {
                WarpingTileProvider::tile(metadata, request)
            } else {
                Err(error)
            }
        })?;
        if raw.data.is_empty() {
            Ok(None)
        } else {
            Ok(Some(AnyDenseArray::from_raster_tile_bytes(&raw.data)?))
        }
    }

    fn value(&self, coordinate: Coordinate) -> PixelValue {
        match &self.data {
            Ok(Some(data)) => {
                let point = crs::lat_lon_to_web_mercator(coordinate);
                if !point.x().is_finite() || !point.y().is_finite() {
                    return PixelValue::NoData;
                }
                let cell = self.reference.point_to_cell(point);
                // Tile selection and projection can round opposite ways at an
                // edge. Check each axis before flattening the cell to an index.
                if !(0..data.rows().0).contains(&cell.row) || !(0..data.columns().0).contains(&cell.col) {
                    return PixelValue::NoData;
                }
                data.cell_value::<f64>(cell)
                    .filter(|value| value.is_finite())
                    .map_or(PixelValue::NoData, PixelValue::Data)
            }
            Ok(None) => PixelValue::NoData,
            Err(_) => PixelValue::Unavailable,
        }
    }

    fn bytes(&self) -> usize {
        self.data.as_ref().ok().and_then(|data| data.as_ref()).map_or(0, |data| {
            data.rows().0 as usize * data.columns().0 as usize * usize::from(data.data_type().bytes())
        })
    }
}

impl PixelValueCache {
    fn contains(&self, key: TileKey) -> bool {
        self.tiles.iter().any(|tile| tile.key == key)
    }

    fn lookup(&mut self, request: &SampleRequest) -> Option<PixelValue> {
        if !request.is_on_raster() {
            return Some(PixelValue::NoData);
        }
        let key = request.key();
        let ix = self.tiles.iter().position(|tile| tile.key == key)?;
        let tile = self.tiles.remove(ix)?;
        // Keep cached samples raw; nodata must be rejected before applying even a zero scale.
        let value = match tile.value(request.coordinate) {
            PixelValue::Data(raw) => {
                let scale = request.source.scale();
                let physical = raw * scale.scale + scale.offset;
                if physical.is_finite() {
                    PixelValue::Data(physical)
                } else {
                    PixelValue::NoData
                }
            }
            value => value,
        };
        self.tiles.push_back(tile);
        Some(value)
    }

    fn insert(&mut self, tile: CachedTile) {
        self.bytes += tile.bytes();
        self.tiles.push_back(tile);
        while self.tiles.len() > 1 && (self.tiles.len() > MAX_CACHED_TILES || self.bytes > MAX_CACHE_BYTES) {
            if let Some(evicted) = self.tiles.pop_front() {
                self.bytes -= evicted.bytes();
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PixelValue {
    CoordinatesOnly,
    Loading,
    Data(f64),
    NoData,
    Unavailable,
}

struct SampleRequest {
    source: Arc<RasterSource>,
    coordinate: Coordinate,
    dpi: u8,
}

impl SampleRequest {
    fn key(&self) -> TileKey {
        let metadata = self.source.metadata();
        TileKey {
            layer: metadata.id,
            tile: Tile::for_coordinate(self.coordinate, metadata.max_zoom.clamp(0, 22)),
            dpi: if metadata.supports_dpi_ratio { self.dpi.clamp(1, 4) } else { 1 },
        }
    }

    fn is_on_raster(&self) -> bool {
        self.source.metadata().bounds().contains_coordinate(&self.coordinate)
    }
}

/// Owns cursor state and one tile read at a time. Current-position cache misses
/// take priority over neighboring-tile prefetches; mouse moves never queue reads.
pub struct PixelInfo {
    position: Option<(Coordinate, Option<LayerId>, u8)>,
    value: PixelValue,
    cache: PixelValueCache,
    current: Option<SampleRequest>,
    neighborhood: Option<TileKey>,
    prefetch: VecDeque<TileKey>,
    task: Option<Task<()>>,
}

impl PixelInfo {
    pub fn new() -> Self {
        Self {
            position: None,
            value: PixelValue::CoordinatesOnly,
            cache: PixelValueCache::default(),
            current: None,
            neighborhood: None,
            prefetch: VecDeque::new(),
            task: None,
        }
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        if self.position.take().is_some() {
            self.current = None;
            self.neighborhood = None;
            self.prefetch.clear();
            cx.notify();
        }
    }

    pub fn show(&mut self, coordinate: Coordinate, source: Option<Arc<RasterSource>>, dpi: u8, cx: &mut Context<Self>) {
        let position = (coordinate, source.as_ref().map(|source| source.metadata().id), dpi);
        // Display identity includes units; raw tile identity deliberately does not.
        let same_units = self.current.as_ref().map(|request| request.source.unscaling_enabled())
            == source.as_ref().filter(|source| source.is_numeric()).map(|source| source.unscaling_enabled());
        if self.position == Some(position) && same_units {
            return;
        }
        let keep_previous = same_units && self.position.is_some_and(|(_, layer, previous_dpi)| layer == position.1 && previous_dpi == dpi);
        self.position = Some(position);
        self.current = None;
        self.value = match source {
            None => PixelValue::CoordinatesOnly,
            Some(source) if source.metadata().band_nr.is_none() => PixelValue::Unavailable,
            Some(source) => {
                let request = SampleRequest { source, coordinate, dpi };
                let value = self
                    .cache
                    .lookup(&request)
                    .unwrap_or(if keep_previous { self.value } else { PixelValue::Loading });
                self.current = Some(request);
                value
            }
        };
        self.prepare_neighbors();
        self.start_load(cx);
        cx.notify();
    }

    fn prepare_neighbors(&mut self) {
        let key = self.current.as_ref().filter(|request| request.is_on_raster()).map(SampleRequest::key);
        if self.neighborhood == key {
            return;
        }
        self.neighborhood = key;
        self.prefetch.clear();
        let (Some(key), Some(request)) = (key, &self.current) else { return };
        let count = 1 << key.tile.z;
        // Edges first, then corners. Do not read tiles outside the raster bounds.
        for (dx, dy) in [(-1, 0), (1, 0), (0, -1), (0, 1), (-1, -1), (1, -1), (-1, 1), (1, 1)] {
            let tile = Tile {
                x: (key.tile.x + dx).rem_euclid(count),
                y: key.tile.y + dy,
                z: key.tile.z,
            };
            let neighbor = TileKey { tile, ..key };
            if (0..count).contains(&tile.y)
                && tile != key.tile
                && tile.bounds().intersects(&request.source.metadata().bounds())
                && !self.prefetch.contains(&neighbor)
            {
                self.prefetch.push_back(neighbor);
            }
        }
    }

    fn start_load(&mut self, cx: &mut Context<Self>) {
        if self.task.is_some() {
            return;
        }
        let Some(request) = self.current.as_ref().filter(|request| request.is_on_raster()) else {
            return;
        };
        let key = request.key();
        let next = if self.cache.contains(key) {
            self.prefetch.iter().copied().find(|key| !self.cache.contains(*key))
        } else {
            Some(key)
        };
        let Some(next) = next else { return };
        self.prefetch.retain(|key| *key != next);
        let source = request.source.clone();
        let work = cx.background_spawn(async move { CachedTile::load(&source, next) });
        self.task = Some(cx.spawn(async move |this, cx| {
            let tile = work.await;
            let _ = this.update(cx, |this, cx| this.finish_load(tile, cx));
        }));
    }

    fn finish_load(&mut self, tile: CachedTile, cx: &mut Context<Self>) {
        self.cache.insert(tile);
        self.task = None;
        // Sample the latest cursor from the completed tile, not the
        // coordinate that originally caused its load.
        if let Some(request) = &self.current
            && let Some(value) = self.cache.lookup(request)
            && self.value != value
        {
            self.value = value;
            cx.notify();
        }
        self.start_load(cx);
    }

    fn text(&self) -> Option<String> {
        let (coordinate, _, _) = self.position?;
        let coordinates = format!("Lat: {:.6}  Lon: {:.6}", coordinate.latitude, coordinate.longitude);
        let value = match self.value {
            PixelValue::CoordinatesOnly | PixelValue::Loading => return Some(coordinates),
            PixelValue::Data(value) => format!("{value:.2}"),
            PixelValue::NoData => "No data".to_owned(),
            PixelValue::Unavailable => "Unavailable".to_owned(),
        };
        Some(format!("{coordinates}  {value}"))
    }
}

impl Render for PixelInfo {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut info = div().id("pixel-info").test_support();
        if let Some(text) = self.text() {
            info = info
                .aria_label(text.clone())
                .px_2()
                .py_1()
                .text_xs()
                .bg(cx.theme().popover)
                .text_color(cx.theme().popover_foreground)
                .border_1()
                .border_color(cx.theme().border)
                .rounded(cx.theme().radius_tokens().sm)
                .child(text);
        }
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::TestAppContext;
    use inf::colormap::ColorMapPreset;

    #[test]
    fn initial_loading_shows_coordinates_without_placeholder_dots() {
        let mut info = PixelInfo::new();
        info.position = Some((Coordinate::latlon(51.0, 4.0), None, 1));
        info.value = PixelValue::Loading;
        assert_eq!(info.text().as_deref(), Some("Lat: 51.000000  Lon: 4.000000"));
    }

    #[test]
    fn pixel_values_have_no_prefix() {
        let mut info = PixelInfo::new();
        info.position = Some((Coordinate::latlon(51.0, 4.0), None, 1));
        info.value = PixelValue::Data(12.34);
        assert_eq!(info.text().as_deref(), Some("Lat: 51.000000  Lon: 4.000000  12.34"));
    }

    impl PixelValueCache {
        fn sample(&mut self, source: &Arc<RasterSource>, coordinate: Coordinate, dpi: u8) -> Result<Option<f64>> {
            let request = SampleRequest {
                source: source.clone(),
                coordinate,
                dpi,
            };
            if self.lookup(&request).is_none() {
                self.insert(CachedTile::load(source, request.key()));
            }
            match self.lookup(&request) {
                Some(PixelValue::Data(value)) => Ok(Some(value)),
                Some(PixelValue::NoData) => Ok(None),
                other => anyhow::bail!("Unexpected sample: {other:?}"),
            }
        }
    }

    #[test]
    fn sampling_tracks_bands_projection_dpi_and_ignores_display_styles() {
        let dir = tempfile::tempdir().unwrap();
        for epsg in [4326, 3857] {
            let path = dir.path().join(format!("raster-{epsg}.tif"));
            crate::raster_source::test_support::write_raster(&path, epsg, 2).unwrap();
            let sources = RasterSource::open(&path).unwrap();
            let coordinate = if epsg == 4326 {
                Coordinate::latlon(-0.15625, 0.15625)
            } else {
                crs::web_mercator_to_lat_lon(geo::Point::new(15625.0, -15625.0))
            };
            let mut cache = PixelValueCache::default();
            for dpi in [1, 2, 3, 4] {
                assert_eq!(cache.sample(&sources[0], coordinate, dpi).unwrap(), Some(74.0), "EPSG {epsg}, DPI {dpi}");
                assert_eq!(cache.sample(&sources[1], coordinate, dpi).unwrap(), Some(84.0));
            }
            let restyled = sources[1].restyled(ColorMapPreset::Blues, (0.0, 1.0)).unwrap();
            assert_eq!(cache.sample(&restyled, coordinate, 2).unwrap(), Some(84.0));
        }
    }

    #[test]
    fn cached_tile_sampling_rejects_outside_cells_and_nonfinite_coordinates() {
        let reference = GeoReference::with_top_left_origin(
            "EPSG:3857",
            geo::RasterSize::with_rows_cols(geo::Rows(4), geo::Columns(4)),
            geo::Point::new(0.0, 1000.0),
            geo::CellSize::square(100.0),
            None::<f64>,
        );
        let data = AnyDenseArray::filled_with(
            Some(42.0),
            geo::ArrayMetadata::sized_with_nodata(reference.raster_size(), None),
            geo::ArrayDataType::Float32,
        );
        let tile = CachedTile {
            key: TileKey {
                layer: LayerId::new(1),
                tile: Tile { x: 2, y: 1, z: 2 },
                dpi: 1,
            },
            reference,
            data: Ok(Some(data)),
        };
        let inside = crs::web_mercator_to_lat_lon(tile.reference.cell_center(geo::Cell::from_row_col(1, 1)));
        assert_eq!(tile.value(inside), PixelValue::Data(42.0));
        for (row, col) in [(-1, 1), (4, 1), (1, -1), (1, 4)] {
            let coordinate = crs::web_mercator_to_lat_lon(tile.reference.cell_center(geo::Cell::from_row_col(row, col)));
            assert_eq!(tile.value(coordinate), PixelValue::NoData, "Cell ({row}, {col})");
        }
        for coordinate in [
            Coordinate::latlon(f64::NAN, 0.0),
            Coordinate::latlon(0.0, f64::NAN),
            Coordinate::latlon(90.0, 0.0),
        ] {
            assert_eq!(tile.value(coordinate), PixelValue::NoData);
        }
    }

    #[test]
    fn sampling_applies_per_band_scale_and_offset_without_changing_cached_raw_values_or_style_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scaled.tif");
        crate::raster_source::test_support::write_raster(&path, 4326, 2).unwrap();
        let dataset = gdal::Dataset::open_ex(
            &path,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE | gdal::GdalOpenFlags::GDAL_OF_RASTER,
                ..Default::default()
            },
        )
        .unwrap();
        for (band_nr, scale, offset) in [(1, 2.5, -11.0), (2, -0.5, 7.0)] {
            let mut band = dataset.rasterband(band_nr).unwrap();
            band.set_scale(scale).unwrap();
            band.set_offset(offset).unwrap();
        }
        drop(dataset);
        let sources = RasterSource::open(&path).unwrap();
        let coordinate = Coordinate::latlon(-0.15625, 0.15625);
        let mut cache = PixelValueCache::default();
        for dpi in [1, 2] {
            for (source, raw, physical) in [(&sources[0], 74.0, 174.0), (&sources[1], 84.0, -35.0)] {
                assert_eq!(cache.sample(source, coordinate, dpi).unwrap(), Some(physical));
                let request = SampleRequest {
                    source: source.clone(),
                    coordinate,
                    dpi,
                };
                let key = request.key();
                let tile = cache.tiles.iter().find(|tile| tile.key == key).unwrap();
                assert_eq!(tile.value(coordinate), PixelValue::Data(raw));
                let retained_tiles = cache.tiles.len();
                let retained_bytes = cache.bytes;
                let restyled = source.restyled(ColorMapPreset::Blues, (0.0, 1.0)).unwrap();
                assert_eq!(restyled.metadata().id, source.metadata().id);
                assert_eq!(cache.sample(&restyled, coordinate, dpi).unwrap(), Some(physical));
                let raw_source = restyled.with_unscaling(false).unwrap();
                assert_eq!(cache.sample(&raw_source, coordinate, dpi).unwrap(), Some(raw));
                assert_eq!(
                    cache.sample(&raw_source.with_unscaling(true).unwrap(), coordinate, dpi).unwrap(),
                    Some(physical)
                );
                assert_eq!((cache.tiles.len(), cache.bytes), (retained_tiles, retained_bytes));
                let tile = cache.tiles.iter().find(|tile| tile.key == key).unwrap();
                assert_eq!(tile.value(coordinate), PixelValue::Data(raw));
            }
        }
    }

    #[test]
    fn sampling_rejects_nonfinite_physical_values_without_changing_the_cached_raw_sample() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scaled.tif");
        crate::raster_source::test_support::write_raster(&path, 4326, 1).unwrap();
        let dataset = gdal::Dataset::open_ex(
            &path,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE | gdal::GdalOpenFlags::GDAL_OF_RASTER,
                ..Default::default()
            },
        )
        .unwrap();
        let mut band = dataset.rasterband(1).unwrap();
        band.set_scale(2.0).unwrap();
        band.set_offset(17.0).unwrap();
        drop(dataset);
        let source = RasterSource::open(&path).unwrap().remove(0);
        let coordinate = Coordinate::latlon(-0.15625, 0.15625);
        let request = SampleRequest { source, coordinate, dpi: 1 };
        let mut tile = CachedTile::load(&request.source, request.key());
        // The fixture is Float32; inject a finite Float64 sample that overflows only after conversion.
        let metadata = *tile.data.as_ref().unwrap().as_ref().unwrap().metadata();
        tile.data = Ok(Some(AnyDenseArray::filled_with(Some(f64::MAX), metadata, geo::ArrayDataType::Float64)));
        assert_eq!(tile.value(coordinate), PixelValue::Data(f64::MAX));
        let mut cache = PixelValueCache::default();
        cache.insert(tile);
        assert_eq!(cache.lookup(&request), Some(PixelValue::NoData));
        assert_eq!(cache.tiles.front().unwrap().value(coordinate), PixelValue::Data(f64::MAX));
    }

    #[test]
    fn scaled_sampling_preserves_nodata_and_maps_valid_raw_zero_to_the_offset() {
        let dir = tempfile::tempdir().unwrap();
        for scale in [0.0, 2.5, -0.5] {
            let path = dir.path().join(format!("scaled-{scale}.tif"));
            crate::raster_source::test_support::write_raster(&path, 4326, 1).unwrap();
            let dataset = gdal::Dataset::open_ex(
                &path,
                gdal::DatasetOptions {
                    open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE | gdal::GdalOpenFlags::GDAL_OF_RASTER,
                    ..Default::default()
                },
            )
            .unwrap();
            let mut band = dataset.rasterband(1).unwrap();
            band.set_scale(scale).unwrap();
            band.set_offset(17.0).unwrap();
            band.write((32, 32), (1, 1), &mut gdal::raster::Buffer::new((1, 1), vec![0.0_f32])).unwrap();
            drop(dataset);
            let source = RasterSource::open(&path).unwrap().remove(0);
            let mut cache = PixelValueCache::default();
            assert_eq!(
                cache.sample(&source, Coordinate::latlon(-0.15625, 0.15625), 1).unwrap(),
                Some(17.0),
                "Scale {scale}"
            );
            assert_eq!(cache.sample(&source, Coordinate::latlon(-0.15625, -8.59375), 1).unwrap(), None, "Scale {scale}");
            assert_eq!(cache.sample(&source, Coordinate::latlon(30.0, 30.0), 1).unwrap(), None, "Scale {scale}");
        }
    }

    #[test]
    fn nodata_and_outside_extent_return_no_value_but_zero_is_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raster.tif");
        crate::raster_source::test_support::write_raster(&path, 4326, 1).unwrap();
        let dataset = gdal::Dataset::open_ex(
            &path,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE | gdal::GdalOpenFlags::GDAL_OF_RASTER,
                ..Default::default()
            },
        )
        .unwrap();
        dataset
            .rasterband(1)
            .unwrap()
            .write((32, 32), (1, 1), &mut gdal::raster::Buffer::new((1, 1), vec![0.0_f32]))
            .unwrap();
        drop(dataset);
        let source = RasterSource::open(&path).unwrap().remove(0);
        let mut cache = PixelValueCache::default();
        assert_eq!(cache.sample(&source, Coordinate::latlon(-0.15625, 0.15625), 1).unwrap(), Some(0.0));
        assert_eq!(cache.sample(&source, Coordinate::latlon(-0.15625, -8.59375), 1).unwrap(), None);
        assert_eq!(cache.sample(&source, Coordinate::latlon(30.0, 30.0), 1).unwrap(), None);
    }

    #[gpui_kit::test]
    fn uncached_moves_keep_the_previous_value_until_the_new_sample_arrives(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raster.tif");
        crate::raster_source::test_support::write_raster(&path, 4326, 2).unwrap();
        let sources = RasterSource::open(&path).unwrap();
        let info = cx.new(|_| PixelInfo::new());
        cx.update(|cx| {
            info.update(cx, |info, cx| {
                info.show(Coordinate::latlon(-0.15625, 0.15625), Some(sources[0].clone()), 1, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|cx| {
            info.update(cx, |info, cx| {
                assert_eq!(info.value, PixelValue::Data(74.0));
                // Simulate eviction so the next position requires a background read.
                info.cache = PixelValueCache::default();
                info.show(Coordinate::latlon(-0.15625, 0.46875), Some(sources[0].clone()), 1, cx);
                assert!(info.task.is_some());
                assert_eq!(info.text().as_deref(), Some("Lat: -0.156250  Lon: 0.468750  74.00"));
            })
        });
        cx.run_until_parked();
        cx.update(|cx| {
            info.update(cx, |info, cx| {
                assert_eq!(info.value, PixelValue::Data(75.0));
                info.show(Coordinate::latlon(-0.15625, 0.46875), Some(sources[1].clone()), 1, cx);
                assert_eq!(info.value, PixelValue::Loading, "Do not carry a stale value into a different band");
            })
        });
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| info.read(cx).value), PixelValue::Data(85.0));
    }

    #[gpui_kit::test]
    fn same_point_unscaling_changes_refresh_cached_values_without_rereading(cx: &mut TestAppContext) {
        for (scale, offset, physical) in [(2.5, -11.0, 174.0), (-0.5, 7.0, -30.0), (0.0, 12.0, 12.0), (1.0, 0.0, 74.0)] {
            assert_same_point_unscaling_change(cx, scale, offset, physical);
        }
    }

    fn assert_same_point_unscaling_change(cx: &mut TestAppContext, scale: f64, offset: f64, physical: f64) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scaled.tif");
        crate::raster_source::test_support::write_raster(&path, 4326, 1).unwrap();
        let dataset = gdal::Dataset::open_ex(
            &path,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE | gdal::GdalOpenFlags::GDAL_OF_RASTER,
                ..Default::default()
            },
        )
        .unwrap();
        let mut band = dataset.rasterband(1).unwrap();
        band.set_scale(scale).unwrap();
        band.set_offset(offset).unwrap();
        dataset.close().unwrap();
        let source = RasterSource::open(&path).unwrap().remove(0);
        let raw = source.with_unscaling(false).unwrap();
        let coordinate = Coordinate::latlon(-0.15625, 0.15625);
        let info = cx.new(|_| PixelInfo::new());
        cx.update(|cx| {
            info.update(cx, |info, cx| {
                info.show(coordinate, Some(source.clone()), 1, cx);
                assert!(info.task.is_some());
                // The in-flight raw read must publish in the newest units.
                info.show(coordinate, Some(raw.clone()), 1, cx);
            })
        });
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| info.read(cx).value), PixelValue::Data(74.0));
        std::fs::remove_file(&path).unwrap();
        cx.update(|cx| info.update(cx, |info, cx| assert_cached_unscaling_toggles(info, &source, &raw, coordinate, physical, cx)));
        cx.run_until_parked();
        assert_eq!(cx.update(|cx| info.read(cx).value), PixelValue::Data(physical));
    }

    fn assert_cached_unscaling_toggles(
        info: &mut PixelInfo,
        source: &Arc<RasterSource>,
        raw: &Arc<RasterSource>,
        coordinate: Coordinate,
        physical: f64,
        cx: &mut Context<PixelInfo>,
    ) {
        let retained_tiles = info.cache.tiles.len();
        let retained_bytes = info.cache.bytes;
        for (snapshot, expected) in [(source, physical), (raw, 74.0), (source, physical)] {
            info.show(coordinate, Some(snapshot.clone()), 1, cx);
            assert_eq!(info.value, PixelValue::Data(expected));
            assert_eq!(info.current.as_ref().unwrap().source.unscaling_enabled(), snapshot.unscaling_enabled());
            assert!(info.task.is_none(), "Cached toggles must not start a new read");
            assert_eq!((info.cache.tiles.len(), info.cache.bytes), (retained_tiles, retained_bytes));
            let key = info.current.as_ref().unwrap().key();
            let tile = info.cache.tiles.iter().find(|tile| tile.key == key).unwrap();
            assert_eq!(tile.value(coordinate), PixelValue::Data(74.0));
        }
    }

    #[gpui_kit::test]
    fn cached_cursor_values_update_without_waiting_for_the_executor(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raster.tif");
        crate::raster_source::test_support::write_raster(&path, 4326, 1).unwrap();
        let source = RasterSource::open(&path).unwrap().remove(0);
        let info = cx.new(|_| PixelInfo::new());
        cx.update(|cx| info.update(cx, |info, cx| info.show(Coordinate::latlon(-0.15625, 0.15625), Some(source.clone()), 1, cx)));
        cx.run_until_parked();
        cx.update(|cx| {
            info.update(cx, |info, cx| {
                for longitude in [0.46875, 0.78125, 0.15625] {
                    info.show(Coordinate::latlon(-0.15625, longitude), Some(source.clone()), 1, cx);
                    assert!(
                        matches!(info.value, PixelValue::Data(_)),
                        "Cached moves must publish the value synchronously, not flash Loading"
                    );
                }
                assert_eq!(info.value, PixelValue::Data(74.0));
                // The first sample prefetched the tile across each boundary at 0°.
                for (coordinate, expected) in [
                    (Coordinate::latlon(-0.15625, -0.15625), 73.0),
                    (Coordinate::latlon(0.15625, -0.15625), 72.0),
                    (Coordinate::latlon(0.15625, 0.15625), 73.0),
                ] {
                    info.show(coordinate, Some(source.clone()), 1, cx);
                    assert_eq!(info.value, PixelValue::Data(expected), "Prefetched tile boundaries must not wait for I/O");
                }
            })
        });
    }

    #[gpui_kit::test]
    fn cached_values_remain_available_during_another_load_and_late_results_stay_hidden(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raster.tif");
        crate::raster_source::test_support::write_raster(&path, 4326, 2).unwrap();
        let sources = RasterSource::open(&path).unwrap();
        let info = cx.new(|_| PixelInfo::new());
        cx.update(|cx| {
            info.update(cx, |info, cx| {
                info.show(Coordinate::latlon(-0.15625, 0.15625), Some(sources[0].clone()), 1, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|cx| {
            info.update(cx, |info, cx| {
                info.show(Coordinate::latlon(-0.15625, 0.15625), Some(sources[1].clone()), 1, cx);
                assert!(info.task.is_some());
                info.show(Coordinate::latlon(-0.15625, 0.46875), Some(sources[0].clone()), 1, cx);
                assert_eq!(
                    info.value,
                    PixelValue::Data(75.0),
                    "An in-flight read must not take cached tiles away from the cursor"
                );
                info.clear(cx);
            })
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(info.read(cx).position.is_none());
            assert!(info.read(cx).current.is_none() && info.read(cx).task.is_none());
            assert!(info.read(cx).text().is_none());
        });
    }

    #[test]
    fn the_tile_cache_bounds_the_number_of_retained_tiles() {
        let mut cache = PixelValueCache::default();
        for x in 0..=MAX_CACHED_TILES {
            let tile = Tile { x: x as i32, y: 0, z: 5 };
            cache.insert(CachedTile {
                key: TileKey {
                    layer: LayerId::new(1),
                    tile,
                    dpi: 1,
                },
                reference: GeoReference::from_tile(&tile, 256, 1),
                data: Ok(None),
            });
        }
        assert_eq!(cache.tiles.len(), MAX_CACHED_TILES);
        assert_eq!(cache.tiles.front().unwrap().key.tile.x, 1);
        assert_eq!(cache.bytes, 0);
    }

    #[gpui_kit::test]
    fn rapid_moves_and_layer_changes_only_publish_the_latest_sample(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raster.tif");
        crate::raster_source::test_support::write_raster(&path, 4326, 2).unwrap();
        let sources = RasterSource::open(&path).unwrap();
        let info = cx.new(|_| PixelInfo::new());
        cx.update(|cx| {
            info.update(cx, |info, cx| {
                info.show(Coordinate::latlon(1.0, 1.0), Some(sources[0].clone()), 1, cx);
                info.show(Coordinate::latlon(2.0, 2.0), Some(sources[0].clone()), 1, cx);
                info.show(Coordinate::latlon(-0.15625, 0.15625), Some(sources[1].clone()), 2, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(info.read(cx).value, PixelValue::Data(84.0));
            assert_eq!(info.read(cx).position.unwrap().0, Coordinate::latlon(-0.15625, 0.15625));
            assert!(info.read(cx).task.is_none());
        });
    }
}
