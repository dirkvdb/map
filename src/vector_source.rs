//! GDAL vector detection and bounds via `geo`, with native on-demand tiled
//! rendering. Other vector formats are converted to WGS84 GeoJSON.

use std::ffi::{CStr, CString, c_char, c_int, c_void};

use std::path::Path;
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result, bail, ensure};
use gdal::Dataset;
use gdal::vector::LayerAccess as _;
use geo::vector::gdalio;
use serde_json::{Value, json};

static NEXT_SOURCE_ID: AtomicU64 = AtomicU64::new(1);
const SOURCE_ID: &str = "local-vector";

/// One vector layer, safe to share between the UI and renderer.
///
/// PMTiles/MBTiles retain only a native source URL and archive bounds: MapLibre loads
/// visible tiles at the requested zoom. Other formats are materialized as WGS84
/// GeoJSON, which can use substantial memory. Unreferenced spatial layers are
/// rejected rather than guessed to be WGS84. Empty GeoJSON layers use `[0; 4]`.
#[derive(Debug)]
pub struct VectorSource {
    id: u64,
    name: String,
    bounds: [f64; 4],
    data: VectorData,
}

#[derive(Debug)]
enum VectorData {
    GeoJson(Value),
    Tiled { url: String },
}

impl VectorSource {
    /// Probes GDAL's vector drivers through `geo`, without extension-based detection.
    /// Returns one source per layer or `None` so the caller can try raster loading.
    /// Tiled sources remain native; GDAL owns layer metadata and extent handling.
    pub fn open(path: &Path) -> Result<Option<Vec<Arc<Self>>>> {
        let Ok(dataset) = gdalio::dataset::open_read_only(path) else {
            return Ok(None);
        };
        // Container drivers such as GeoPackage can open in vector mode even
        // when the file only contains rasters. Preserve the raster fallback.
        if dataset.layer_count() == 0 {
            return Ok(None);
        }
        let driver = dataset.driver().short_name();
        if matches!(driver.as_str(), "PMTiles" | "MBTiles") {
            let absolute = path.canonicalize().with_context(|| format!("Could not resolve {}", path.display()))?;
            let file_url = url::Url::from_file_path(&absolute).map_err(|_| anyhow::anyhow!("Could not create a file URL for {}", absolute.display()))?;
            let url = match driver.as_str() {
                "PMTiles" => format!("pmtiles://{file_url}"),
                // Native MBTiles takes an encoded absolute path, not a file URL.
                _ => format!("mbtiles://{}", file_url.path()),
            };
            // Reading names uses GDAL's layer metadata, never feature iteration
            // or extent/statistics calculation, which could scan the archive.
            let sources = (0..dataset.layer_count())
                .map(|index| {
                    let layer = dataset.layer(index)?;
                    Ok(Arc::new(Self {
                        id: NEXT_SOURCE_ID.fetch_add(1, Ordering::Relaxed),
                        name: layer.name(),
                        bounds: bounds_array(gdalio::layer_bounds(&layer)?.unwrap_or_else(geo::LatLonBounds::world)),
                        data: VectorData::Tiled { url: url.clone() },
                    }))
                })
                .collect::<Result<Vec<_>>>()?;
            return Ok(Some(sources));
        }

        let directory = tempfile::tempdir().context("Could not create vector conversion directory")?;
        let mut sources = Vec::new();
        for index in 0..dataset.layer_count() {
            let layer = dataset.layer(index)?;
            let name = layer.name();
            ensure!(
                layer.defn().geom_fields().count() <= 1,
                "Layer {name:?} has multiple geometry fields; GeoJSON can only preserve one"
            );
            ensure!(
                layer.defn().geom_fields().count() == 0 || layer.spatial_ref().is_some(),
                "Layer {name:?} has no coordinate reference system"
            );
            let expected_count = layer.try_feature_count();
            let output = directory.path().join(format!("layer-{index}.geojson"));
            translate_layer(&dataset, &name, &output).with_context(|| format!("Could not convert layer {name:?} in {} to WGS84 GeoJSON", path.display()))?;
            let mut data: Value = serde_json::from_reader(std::fs::File::open(&output)?).with_context(|| format!("Could not read converted layer {name:?}"))?;
            ensure!(data["type"] == "FeatureCollection", "GDAL did not produce a FeatureCollection for {name:?}");
            let features = data["features"].as_array_mut().context("GeoJSON features must be an array")?;
            if let Some(expected) = expected_count {
                ensure!(
                    features.len() as u64 == expected,
                    "Layer {name:?}: expected {expected} features, converted {}",
                    features.len()
                );
            }
            let fallback = if features.is_empty() {
                geo::LatLonBounds::from([0.0; 4])
            } else {
                geo::LatLonBounds::world()
            };
            let converted = gdalio::dataset::open_read_only(&output)?;
            // Conversion has already materialized these features. Let GDAL
            // compute their extent; tiled archives never take this scan path.
            let bounds = bounds_array(gdalio::layer_bounds_with_scan(&converted.layer(0)?)?.unwrap_or(fallback));
            sources.push(Arc::new(Self {
                id: NEXT_SOURCE_ID.fetch_add(1, Ordering::Relaxed),
                name,
                bounds,
                data: VectorData::GeoJson(data),
            }));
        }
        Ok(Some(sources))
    }

    /// A process-local identity, independent of raster-source identities.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The original archive metadata or GDAL layer name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// WGS84 `[west, south, east, north]`, as expected by `MapCamera::fit_bounds`.
    /// Bounds and reprojection come from GDAL through `geo`. When no fast extent
    /// is available, use world bounds rather than scanning a tiled archive.
    pub fn bounds(&self) -> [f64; 4] {
        self.bounds
    }

    /// A native tiled source for PMTiles/MBTiles, otherwise inline GeoJSON.
    pub fn source_json(&self) -> Value {
        match &self.data {
            VectorData::Tiled { url } => json!({ "type": "vector", "url": url }),
            VectorData::GeoJson(data) => json!({ "type": "geojson", "data": data }),
        }
    }

    /// Fill, line, and circle layers referencing the fixed source `local-vector`.
    ///
    /// Layer IDs include this source's identity; the caller must install the source
    /// under `local-vector` (or rewrite source references to display several at once).
    /// MapLibre's geometry-type filter also matches corresponding multi-geometries.
    /// The caller supplies a CSS color resolved from the application theme.
    pub fn layers_json(&self, opacity: f32, color: &str) -> Vec<Value> {
        let opacity = if opacity.is_finite() { opacity.clamp(0.0, 1.0) } else { 1.0 };
        let mut layers = vec![
            json!({
                "id": format!("local-vector-{}-fill", self.id),
                "type": "fill", "source": SOURCE_ID,
                "filter": ["==", ["geometry-type"], "Polygon"],
                "paint": { "fill-color": color, "fill-opacity": opacity }
            }),
            json!({
                "id": format!("local-vector-{}-line", self.id),
                "type": "line", "source": SOURCE_ID,
                "filter": ["match", ["geometry-type"], ["LineString", "Polygon"], true, false],
                "paint": { "line-color": color, "line-width": 1.5, "line-opacity": opacity }
            }),
            json!({
                "id": format!("local-vector-{}-circle", self.id),
                "type": "circle", "source": SOURCE_ID,
                "filter": ["==", ["geometry-type"], "Point"],
                "paint": { "circle-color": color, "circle-radius": 4, "circle-opacity": opacity }
            }),
        ];
        if matches!(self.data, VectorData::Tiled { .. }) {
            for layer in &mut layers {
                layer["source-layer"] = json!(self.name);
            }
        }
        layers
    }
}

fn bounds_array(bounds: geo::LatLonBounds) -> [f64; 4] {
    [bounds.west(), bounds.south(), bounds.east(), bounds.north()]
}

// gdal 0.19 does not wrap GDALVectorTranslate. These are the public gdal_utils.h
// signatures, using opaque handles so no additional gdal-sys dependency is needed.
unsafe extern "C" {
    fn GDALVectorTranslateOptionsNew(arguments: *mut *mut c_char, binary_options: *mut c_void) -> *mut c_void;
    fn GDALVectorTranslateOptionsFree(options: *mut c_void);
    fn GDALVectorTranslate(
        destination: *const c_char,
        destination_dataset: *mut c_void,
        source_count: c_int,
        sources: *mut *mut c_void,
        options: *const c_void,
        usage_error: *mut c_int,
    ) -> *mut c_void;
    fn CPLErrorReset();
    fn CPLGetLastErrorType() -> c_int;
    fn CPLGetLastErrorMsg() -> *const c_char;
}

struct TranslateOptions(NonNull<c_void>);

impl Drop for TranslateOptions {
    fn drop(&mut self) {
        // SAFETY: This is an owned options handle, freed exactly once.
        unsafe { GDALVectorTranslateOptionsFree(self.0.as_ptr()) };
    }
}

fn gdal_error() -> String {
    // SAFETY: CPL returns a non-null, thread-local, NUL-terminated string. Copy it
    // immediately, before another GDAL call can invalidate the borrowed storage.
    unsafe { CStr::from_ptr(CPLGetLastErrorMsg()) }.to_string_lossy().into_owned()
}

fn translate_layer(dataset: &Dataset, layer: &str, output: &Path) -> Result<()> {
    let arguments = [
        "-f",
        "GeoJSON",
        "-t_srs",
        // CRS84 is WGS84 with traditional GIS (longitude, latitude) axis order.
        "OGC:CRS84",
        "-lco",
        "SIGNIFICANT_FIGURES=17",
        "-lco",
        "COORDINATE_PRECISION=17",
        "-preserve_fid",
        layer,
    ];
    let arguments: Vec<CString> = arguments.into_iter().map(CString::new).collect::<std::result::Result<_, _>>()?;
    let mut pointers: Vec<*mut c_char> = arguments.iter().map(|argument| argument.as_ptr().cast_mut()).collect();
    pointers.push(ptr::null_mut());
    // SAFETY: argv is NUL-terminated and its strings live throughout this call.
    // GDAL parses but does not modify the string contents or retain argv.
    let options = unsafe { GDALVectorTranslateOptionsNew(pointers.as_mut_ptr(), ptr::null_mut()) };
    let options = TranslateOptions(NonNull::new(options).with_context(|| format!("Invalid GDAL translation options: {}", gdal_error()))?);
    // The generated temporary filename need not preserve the input's encoding.
    let destination = CString::new(output.to_str().context("Non-UTF-8 vector conversion directory")?)?;
    let mut source = dataset.c_dataset();
    let mut usage_error = 0;
    // SAFETY: Resets only this thread's GDAL error state. A read failure must not
    // masquerade as end-of-layer even if GDAL returns a non-null output handle.
    unsafe { CPLErrorReset() };
    // SAFETY: Source is borrowed for this synchronous call, options is live, and
    // all pointers have the types/lifetimes required by GDALVectorTranslate.
    let result = unsafe { GDALVectorTranslate(destination.as_ptr(), ptr::null_mut(), 1, &mut source, options.0.as_ptr(), &mut usage_error) };
    if result.is_null() {
        bail!("GDAL vector translation failed (usage error {usage_error}): {}", gdal_error());
    }
    // SAFETY: A non-null result is a newly owned dataset; the Rust wrapper closes
    // it on every exit path, including a flush error.
    let mut output_dataset = unsafe { Dataset::from_c_dataset(result) };
    // SAFETY: Reads this thread's error state; CE_Failure = 3, CE_Fatal = 4.
    ensure!(unsafe { CPLGetLastErrorType() } < 3, "GDAL vector conversion failed: {}", gdal_error());
    ensure!(usage_error == 0, "Invalid GDAL vector translation arguments");
    output_dataset.flush_cache()?;
    drop(output_dataset);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_mbtiles(path: &Path) -> Result<()> {
        let connection = sqlite::Connection::new(path, sqlite::AccessMode::Create)?;
        connection.execute("CREATE TABLE metadata (name TEXT, value TEXT)")?;
        connection.execute("CREATE TABLE tiles (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_data BLOB)")?;
        let mut tile = Vec::new();
        for name in ["points", "polygons"] {
            let mut layer = vec![0x0a, name.len() as u8];
            layer.extend_from_slice(name.as_bytes());
            layer.extend_from_slice(&[0x28, 0x80, 0x20, 0x78, 0x02]);
            tile.extend_from_slice(&[0x1a, layer.len() as u8]);
            tile.extend(layer);
        }
        let mut insert = connection.prepare_statement("INSERT INTO tiles VALUES (0,0,0,?1) RETURNING zoom_level")?;
        insert.bind_blob(1, &tile)?;
        ensure!(insert.next().is_some(), "Could not insert fixture tile");
        drop(insert);
        let layers = json!({"vector_layers": [{"id": "points", "fields": {}}, {"id": "polygons", "fields": {}}]}).to_string();
        for (name, value) in [
            ("format", "pbf"),
            ("json", layers.as_str()),
            ("minzoom", "0"),
            ("maxzoom", "0"),
            ("bounds", "-4,49,6,53"),
        ] {
            let statement = connection.prepare_statement("INSERT INTO metadata VALUES (?, ?)")?;
            statement.bind_text(1, name)?;
            statement.bind_text(2, value)?;
            statement.step();
        }
        Ok(())
    }

    #[test]
    fn mbtiles_uses_native_urls_and_metadata_without_materializing_features() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("vector # café.mbtiles");
        write_mbtiles(&path)?;
        let sources = VectorSource::open(&path)?.context("Expected GDAL MBTiles driver to expose vector layers")?;
        assert_eq!(sources.iter().map(|source| source.name()).collect::<Vec<_>>(), ["points", "polygons"]);
        assert_ne!(sources[0].id(), sources[1].id());
        assert_eq!(sources[0].source_json(), sources[1].source_json());
        for source in sources {
            assert!(matches!(source.data, VectorData::Tiled { .. }));
            for (actual, expected) in source.bounds().into_iter().zip([-4.0, 49.0, 6.0, 53.0]) {
                assert!((actual - expected).abs() < 1e-8);
            }
            let json = source.source_json();
            assert_eq!(json["type"], "vector");
            assert!(json.get("data").is_none(), "MBTiles must never become GeoJSON");
            let native_url = json["url"].as_str().unwrap();
            let encoded_path = native_url.strip_prefix("mbtiles://").unwrap();
            assert!(encoded_path.starts_with('/'));
            assert!(!encoded_path.starts_with("file:"));
            assert_eq!(
                url::Url::parse(&format!("file://{encoded_path}"))?.to_file_path().unwrap(),
                path.canonicalize()?
            );
            assert!(native_url.contains("%20%23%20caf%C3%A9"));
            for layer in source.layers_json(0.5, "red") {
                assert_eq!(layer["source-layer"], source.name());
            }
        }
        Ok(())
    }

    fn write_empty_pmtiles(path: &Path) -> Result<()> {
        let metadata = serde_json::to_vec(&json!({"vector_layers": [
            {"id": "points", "fields": {}}, {"id": "polygons", "fields": {}}
        ]}))?;
        let mut header = [0_u8; 127];
        header[..8].copy_from_slice(b"PMTiles\x03");
        for (offset, value) in [
            (8, 127_u64),
            (16, 1),
            (24, 128),
            (32, metadata.len() as u64),
            (40, 128 + metadata.len() as u64),
            (56, 128 + metadata.len() as u64),
        ] {
            header[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        header[96..102].copy_from_slice(&[1, 1, 1, 1, 0, 14]);
        for (offset, value) in [(102, -40_000_000_i32), (106, 490_000_000), (110, 60_000_000), (114, 530_000_000)] {
            header[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        let mut bytes = header.to_vec();
        bytes.push(0); // Empty root directory.
        bytes.extend(metadata);
        std::fs::write(path, bytes)?;
        Ok(())
    }

    #[test]
    fn pmtiles_uses_native_urls_and_metadata_without_materializing_features() -> Result<()> {
        let directory = tempfile::tempdir()?;
        // URL escaping matters for local paths containing URL delimiters or Unicode.
        let path = directory.path().join("vector # café.pmtiles");
        write_empty_pmtiles(&path)?;
        let sources = VectorSource::open(&path)?.context("Expected native PMTiles layers")?;
        assert_eq!(sources.iter().map(|source| source.name()).collect::<Vec<_>>(), ["points", "polygons"]);
        for source in sources {
            assert!(matches!(source.data, VectorData::Tiled { .. }));
            for (actual, expected) in source.bounds().into_iter().zip([-4.0, 49.0, 6.0, 53.0]) {
                assert!((actual - expected).abs() < 1e-8);
            }
            let json = source.source_json();
            assert_eq!(json["type"], "vector");
            assert!(json.get("data").is_none());
            let native_url = json["url"].as_str().unwrap();
            let file_url = url::Url::parse(native_url.strip_prefix("pmtiles://").unwrap())?;
            assert_eq!(file_url.to_file_path().unwrap(), path.canonicalize()?);
            assert!(native_url.contains("%20%23%20caf%C3%A9"));
            for layer in source.layers_json(0.5, "red") {
                assert_eq!(layer["source-layer"], source.name());
            }
        }
        Ok(())
    }

    fn open_geojson(data: Value) -> Result<Vec<Arc<VectorSource>>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("features.geojson");
        std::fs::write(&path, serde_json::to_vec(&data)?)?;
        VectorSource::open(&path)?.context("Expected a vector dataset")
    }

    #[test]
    fn open_preserves_geometry_and_properties() -> Result<()> {
        let geometries = [
            json!({"type": "Point", "coordinates": [4.0, 51.0, 7.0]}),
            json!({"type": "LineString", "coordinates": [[2.0, 49.0], [6.0, 53.0]]}),
            json!({"type": "Polygon", "coordinates": [[[3.0, 50.0], [5.0, 50.0], [5.0, 52.0], [3.0, 50.0]]]}),
            json!({"type": "MultiPoint", "coordinates": [[3.0, 50.0], [5.0, 52.0]]}),
            json!({"type": "MultiLineString", "coordinates": [[[3.0, 50.0], [5.0, 52.0]]]}),
            json!({"type": "MultiPolygon", "coordinates": [[[[3.0, 50.0], [5.0, 50.0], [5.0, 52.0], [3.0, 50.0]]]]}),
            json!({"type": "GeometryCollection", "geometries": [{"type": "Point", "coordinates": [4.0, 51.0]}]}),
            Value::Null,
        ];
        let properties = json!({"name": "café", "count": 1234567890123_i64, "value": 1.25, "active": true,
            "missing": null, "tags": ["one", "two"], "nested": {"value": 2}});
        let features: Vec<Value> = geometries
            .iter()
            .enumerate()
            .map(|(id, geometry)| json!({"type": "Feature", "id": id, "geometry": geometry, "properties": properties}))
            .collect();
        let sources = open_geojson(json!({"type": "FeatureCollection", "features": features}))?;
        let exported = sources[0].source_json();
        assert_eq!(exported["type"], "geojson");
        for (actual, expected) in exported["data"]["features"].as_array().unwrap().iter().zip(&features) {
            assert_eq!(actual["geometry"], expected["geometry"]);
            assert_eq!(actual["properties"], expected["properties"]);
            assert_eq!(actual["id"], expected["id"]);
        }
        assert_eq!(exported["data"]["features"].as_array().unwrap().len(), features.len());
        assert_eq!(sources[0].bounds(), [2.0, 49.0, 6.0, 53.0]);
        assert_eq!(sources[0].name(), "features");
        Ok(())
    }

    #[test]
    fn open_reprojects_mercator_to_longitude_latitude() -> Result<()> {
        let sources = open_geojson(json!({
            "type": "FeatureCollection", "crs": {"type": "name", "properties": {"name": geo::crs::epsg::WGS84_WEB_MERCATOR.to_string()}},
            "features": [{"type": "Feature", "properties": {"value": 42},
                "geometry": {"type": "Point", "coordinates": [445277.96317309426, 6621293.722740169]}}]
        }))?;
        let [west, south, east, north] = sources[0].bounds();
        assert!((west - 4.0).abs() < 1e-9 && (south - 51.0).abs() < 1e-9);
        assert_eq!([west, south], [east, north]);
        assert_eq!(sources[0].source_json()["data"]["features"][0]["properties"]["value"], 42);
        Ok(())
    }

    #[test]
    fn open_rejects_raster_and_unknown_input() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("raster.tif");
        let raster = gdal::DriverManager::get_driver_by_name("GTiff")?.create_with_band_type::<u8, _>(&path, 2, 2, 1)?;
        drop(raster);
        assert!(VectorSource::open(&path)?.is_none());
        let unknown = directory.path().join("unknown.dat");
        std::fs::write(&unknown, "not a dataset")?;
        assert!(VectorSource::open(&unknown)?.is_none());
        Ok(())
    }

    #[test]
    fn empty_layer_has_finite_bounds_and_style_ids_are_unique() -> Result<()> {
        let data = json!({"type": "FeatureCollection", "features": []});
        let first = open_geojson(data.clone())?;
        let second = open_geojson(data)?;
        assert_eq!(first[0].bounds(), [0.0; 4]);
        assert_ne!(first[0].id(), second[0].id());
        let layers = first[0].layers_json(0.4, "#3b82f6");
        let other_layers = second[0].layers_json(0.4, "#3b82f6");
        assert_eq!(
            layers.iter().map(|layer| layer["type"].as_str().unwrap()).collect::<Vec<_>>(),
            ["fill", "line", "circle"]
        );
        for (layer, other) in layers.iter().zip(other_layers) {
            assert_eq!(layer["source"], SOURCE_ID);
            assert_ne!(layer["id"], other["id"]);
            assert!(layer["filter"].is_array());
            let key = format!("{}-opacity", layer["type"].as_str().unwrap());
            assert!((layer["paint"][key].as_f64().unwrap() - 0.4).abs() < 1e-6);
        }
        Ok(())
    }

    #[test]
    fn open_loads_every_layer_with_distinct_identities() -> Result<()> {
        use gdal::spatial_ref::SpatialRef;
        use gdal::vector::{Geometry, LayerOptions};

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("layers.gpkg");
        let mut dataset = gdal::DriverManager::get_driver_by_name("GPKG")?.create_vector_only(&path)?;
        let srs = SpatialRef::from_epsg(u32::from(geo::crs::epsg::WGS84.code()))?;
        for name in ["first", "second"] {
            let mut layer = dataset.create_layer(LayerOptions {
                name,
                srs: Some(&srs),
                ..Default::default()
            })?;
            layer.create_feature(Geometry::from_wkt("POINT (4 51)")?)?;
        }
        drop(dataset);
        let sources = VectorSource::open(&path)?.context("Expected vector layers")?;
        assert_eq!(sources.iter().map(|source| source.name()).collect::<Vec<_>>(), ["first", "second"]);
        assert_ne!(sources[0].id(), sources[1].id());
        for source in sources {
            assert_eq!(source.bounds(), [4.0, 51.0, 4.0, 51.0]);
        }
        Ok(())
    }

    #[test]
    fn open_errors_for_spatial_layer_without_crs() -> Result<()> {
        use gdal::vector::{Geometry, LayerOptions};

        let directory = tempfile::tempdir()?;
        let path = directory.path().join("unreferenced.shp");
        let mut dataset = gdal::DriverManager::get_driver_by_name("ESRI Shapefile")?.create_vector_only(&path)?;
        let mut layer = dataset.create_layer(LayerOptions::default())?;
        layer.create_feature(Geometry::from_wkt("POINT (4 51)")?)?;
        drop(dataset);
        let error = VectorSource::open(&path).unwrap_err();
        assert!(error.to_string().contains("no coordinate reference system"), "{error:#}");
        Ok(())
    }

    #[test]
    #[ignore = "requires Dirk's exact local PMTiles file and the GDAL PMTiles driver"]
    fn open_local_wbgt_pmtiles() -> Result<()> {
        let path = Path::new("/home/dirk/Downloads/AD_WBGT_20240620-20240626_construction.pmtiles");
        let sources = VectorSource::open(path)?.context("Local PMTiles was not recognized as vector")?;
        ensure!(!sources.is_empty(), "No PMTiles layers");
        assert_eq!(sources[0].name(), "construction");
        for source in sources {
            let data = source.source_json();
            assert_eq!(data["type"], "vector");
            assert_eq!(data["url"], format!("pmtiles://{}", url::Url::from_file_path(path).unwrap()));
            assert!(data.get("data").is_none(), "PMTiles must not be materialized as GeoJSON");
            let dataset = gdalio::dataset::open_read_only(path)?;
            assert_eq!(
                source.bounds(),
                bounds_array(gdalio::layer_bounds(&dataset.layer_by_name(source.name())?)?.unwrap_or_else(geo::LatLonBounds::world))
            );
            for layer in source.layers_json(1.0, "red") {
                assert_eq!(layer["source-layer"], source.name());
            }
        }
        Ok(())
    }
}
