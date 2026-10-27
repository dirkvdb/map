use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::marker::PhantomData;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context as _, Result, ensure};
use gdal::Dataset;
use gdal::vector::LayerAccess as _;
use geo::vector::{VectorFileFormat, algo, gdalio};
use inf::colormap::{ColorMapDirection, ColorMapPreset, ProcessedColorMap};
use serde_json::{Value, json};

static NEXT_SOURCE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_STYLE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_CONVERSION_ID: AtomicU64 = AtomicU64::new(1);
const SOURCE_ID: &str = "local-vector";
const PALETTE_STOPS: usize = 256;
const OGR2OGR_ARROW_API: &CStr = c"OGR2OGR_USE_ARROW_API";

/// Synchronous progress while loading a vector dataset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VectorLoadProgress {
    /// GDAL conversion fraction, aggregated equally across dataset layers.
    Converting(f64),
    /// The current layer has finished translation and is being finalized,
    /// validated, and prepared. A later layer can resume `Converting`.
    Preparing,
}

struct ConversionProgress<'a> {
    callback: &'a mut dyn FnMut(VectorLoadProgress),
    per_feature: bool,
}

impl ConversionProgress<'_> {
    fn report_translation(&mut self, fraction: f64, preparing: &mut bool) {
        if self.per_feature && *preparing {
            return;
        }
        (self.callback)(VectorLoadProgress::Converting(fraction));
        if self.per_feature && fraction >= 1.0 {
            *preparing = true;
            // Emit inside GDAL's final callback, before the translator
            // returns and any output finalization or extent scans run.
            (self.callback)(VectorLoadProgress::Preparing);
        }
    }
}

/// A top-level numeric attribute and its usable default coloring range.
#[derive(Debug)]
pub struct NumericProperty {
    name: String,
    range: (f64, f64),
}

impl NumericProperty {
    /// The attribute name as stored in feature properties.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Finite increasing bounds; constants are expanded, unknown tiled ranges
    /// use `(0, 1)` without scanning any tiles.
    pub fn range(&self) -> (f64, f64) {
        self.range
    }
}

/// One vector layer, safe to share between the UI and renderer.
///
/// PMTiles/MBTiles retain only a native source URL and archive bounds: MapLibre loads
/// visible tiles at the requested zoom. Other formats are materialized as WGS84
/// GeoJSON, which can use substantial memory. Unreferenced spatial layers are
/// rejected rather than guessed to be WGS84, except CSV lat/lon columns, which
/// establish WGS84 coordinates when no CRS is declared. Empty layers use `[0; 4]`.
#[derive(Debug, Clone)]
pub struct VectorSource {
    id: u64,
    style_id: u64,
    name: String,
    bounds: [f64; 4],
    data: VectorData,
    points_only: bool,
    numeric_properties: Arc<[NumericProperty]>,
    property: Option<usize>,
    palette: ColorMapPreset,
    range: Option<(f64, f64)>,
}

#[derive(Debug, Clone)]
enum VectorData {
    GeoJson(Arc<String>),
    Tiled { url: String },
}

impl VectorSource {
    /// Probes GDAL's vector drivers through `geo`, without extension-based detection.
    /// Returns one source per layer or `None` so the caller can try raster loading.
    /// Tiled sources remain native; GDAL owns layer metadata and extent handling.
    #[cfg(test)]
    pub fn open(path: &Path) -> Result<Option<Vec<Arc<Self>>>> {
        Self::open_with_optional_progress(path, None)
    }

    /// Opens a dataset with synchronous GDAL conversion progress, aggregated
    /// equally across its layers. Fractions describe reprojection/conversion,
    /// not subsequent validation or numeric-metadata preparation. Native tiled
    /// sources do not run conversion or invoke the callback.
    #[cfg(test)]
    pub fn open_with_progress(path: &Path, progress: &mut dyn FnMut(f64)) -> Result<Option<Vec<Arc<Self>>>> {
        Self::open_with_optional_progress(
            path,
            Some(ConversionProgress {
                callback: &mut |event| {
                    if let VectorLoadProgress::Converting(fraction) = event {
                        progress(fraction);
                    }
                },
                per_feature: false,
            }),
        )
    }

    /// Opens a dataset with conversion and preparation phases on the calling thread.
    /// Materialized vectors start with `Converting(0.0)`. Conversion is aggregated
    /// equally across layers; each layer switches to `Preparing` at GDAL completion,
    /// before closing/reopening output, scanning bounds, or preparing Rust metadata.
    /// Drivers without completion callbacks switch when translation returns.
    /// Later layers resume `Converting`; native tiled sources emit no events.
    ///
    /// Only this API disables GDAL's Arrow translation path, using a thread-local
    /// override during translation to obtain per-feature progress. The previous
    /// override is restored even on error or unwind; global configuration is untouched.
    ///
    /// # Errors
    /// Returns an error if conversion, validation, or metadata preparation fails.
    /// Returns `None` for inputs GDAL cannot open as a vector dataset.
    pub fn open_with_load_progress(path: &Path, progress: &mut dyn FnMut(VectorLoadProgress)) -> Result<Option<Vec<Arc<Self>>>> {
        Self::open_with_optional_progress(
            path,
            Some(ConversionProgress {
                callback: progress,
                per_feature: true,
            }),
        )
    }

    fn open_with_optional_progress(path: &Path, mut progress: Option<ConversionProgress<'_>>) -> Result<Option<Vec<Arc<Self>>>> {
        let load_started = Instant::now();
        let opened = Instant::now();
        let Ok(dataset) = gdalio::dataset::open_read_only(path) else {
            return Ok(None);
        };
        log::info!(target: "map::vector_load", "path={path:?} stage=gdal-open elapsed={:?}", opened.elapsed());
        // Container drivers such as GeoPackage can open in vector mode even
        // when the file only contains rasters. Preserve the raster fallback.
        if dataset.layer_count() == 0 {
            return Ok(None);
        }
        let driver = dataset.driver().short_name();
        let is_csv = driver == VectorFileFormat::Csv.gdal_driver_name();
        let dataset = if is_csv { open_csv_dataset(path)? } else { dataset };
        if [VectorFileFormat::PMTiles.gdal_driver_name(), VectorFileFormat::MBTiles.gdal_driver_name()].contains(&driver.as_str()) {
            return Self::tiled_sources(&dataset, path, &driver).map(Some);
        }

        if let Some(progress) = progress.as_mut().filter(|progress| progress.per_feature) {
            (progress.callback)(VectorLoadProgress::Converting(0.0));
        }
        let mut sources = Vec::new();
        for index in 0..dataset.layer_count() {
            let (source, feature_count) = Self::materialize_layer(&dataset, index, path, is_csv, &mut progress)?;
            sources.push(Arc::new(source));
            let source = &sources[sources.len() - 1];
            log::info!(target: "map::vector_load", "path={path:?} source={} layer={:?} stage=layer-prepared since_open={:?} features={feature_count}", source.id(), source.name(), load_started.elapsed());
        }
        let closed = Instant::now();
        dataset.close()?;
        log::info!(target: "map::vector_load", "path={path:?} stage=close-input elapsed={:?} total_open={:?}", closed.elapsed(), load_started.elapsed());
        Ok(Some(sources))
    }

    fn tiled_sources(dataset: &Dataset, path: &Path, driver: &str) -> Result<Vec<Arc<Self>>> {
        let absolute = path.canonicalize().with_context(|| format!("Could not resolve {}", path.display()))?;
        let file_url = url::Url::from_file_path(&absolute).map_err(|_| anyhow::anyhow!("Could not create a file URL for {}", absolute.display()))?;
        let url = if driver == VectorFileFormat::PMTiles.gdal_driver_name() {
            format!("pmtiles://{file_url}")
        } else {
            // Native MBTiles takes an encoded absolute path, not a file URL.
            format!("mbtiles://{}", file_url.path())
        };
        // Reading names uses GDAL's layer metadata, never feature iteration
        // or extent/statistics calculation, which could scan the archive.
        (0..dataset.layer_count())
            .map(|index| {
                let layer = dataset.layer(index)?;
                Ok(Arc::new(Self::new(
                    layer.name(),
                    bounds_array(gdalio::layer_bounds(&layer)?.unwrap_or_else(geo::LatLonBounds::world)),
                    VectorData::Tiled { url: url.clone() },
                    schema_numeric_properties(layer.defn()),
                    schema_points_only(layer.defn()),
                )))
            })
            .collect()
    }

    fn materialize_layer(dataset: &Dataset, index: usize, path: &Path, is_csv: bool, progress: &mut Option<ConversionProgress<'_>>) -> Result<(Self, usize)> {
        let mut layer = dataset.layer(index)?;
        let name = layer.name();
        ensure!(
            layer.defn().geom_fields().count() <= 1,
            "Layer {name:?} has multiple geometry fields; GeoJSON can only preserve one"
        );
        let csv_fields = if is_csv { Some(prepare_csv_layer(&mut layer, path)?) } else { None };
        let assign_wgs84 = is_csv && layer.spatial_ref().is_none();
        ensure!(
            layer.defn().geom_fields().count() == 0 || layer.spatial_ref().is_some() || assign_wgs84,
            "Layer {name:?} has no coordinate reference system"
        );
        let expected_count = layer.try_feature_count();
        let (geojson, bounds) = translate_dataset_layer(dataset, &name, index, assign_wgs84, is_csv, progress)
            .with_context(|| format!("Could not convert layer {name:?} in {} to WGS84 GeoJSON", path.display()))?;
        Self::from_converted_geojson(name, geojson, bounds, expected_count, csv_fields.as_deref())
    }

    fn from_converted_geojson(
        name: String,
        mut geojson: String,
        bounds: Option<geo::LatLonBounds>,
        expected_count: Option<u64>,
        csv_fields: Option<&[String]>,
    ) -> Result<(Self, usize)> {
        let parsed = Instant::now();
        let mut data: Value = serde_json::from_str(&geojson).with_context(|| format!("Could not read converted layer {name:?}"))?;
        log::info!(target: "map::vector_load", "layer={name:?} stage=rust-json-parse elapsed={:?} bytes={}", parsed.elapsed(), geojson.len());
        let validated = Instant::now();
        ensure!(data["type"] == "FeatureCollection", "GDAL did not produce a FeatureCollection for {name:?}");
        let features = data["features"].as_array_mut().context("GeoJSON features must be an array")?;
        if let Some(expected) = expected_count {
            ensure!(
                features.len() as u64 == expected,
                "Layer {name:?}: expected {expected} features, converted {}",
                features.len()
            );
        }
        let properties_changed = match csv_fields {
            Some(fields) => restore_csv_properties(features, fields, &name)?,
            None => false,
        };
        let fallback = if features.is_empty() {
            geo::LatLonBounds::from([0.0; 4])
        } else {
            geo::LatLonBounds::world()
        };
        let feature_count = features.len();
        log::info!(target: "map::vector_load", "layer={name:?} stage=validate-features elapsed={:?} features={feature_count}", validated.elapsed());
        let metadata_started = Instant::now();
        let numeric_properties = geojson_numeric_properties(features);
        let points_only = geojson_points_only(features);
        log::info!(target: "map::vector_load", "layer={name:?} stage=numeric-and-geometry-metadata elapsed={:?} numeric_properties={}", metadata_started.elapsed(), numeric_properties.len());
        // Retain GDAL's original text unless CSV null restoration changed it.
        // The temporary JSON tree is only needed for validation and metadata.
        if properties_changed {
            let serialized = Instant::now();
            geojson = serde_json::to_string(&data)?;
            log::info!(target: "map::vector_load", "layer={name:?} stage=csv-json-reserialize elapsed={:?}", serialized.elapsed());
        }
        let released = Instant::now();
        drop(data);
        log::info!(target: "map::vector_load", "layer={name:?} stage=release-rust-json-tree elapsed={:?}", released.elapsed());
        let bounds = bounds_array(bounds.unwrap_or(fallback));
        Ok((
            Self::new(name, bounds, VectorData::GeoJson(Arc::new(geojson)), numeric_properties, points_only),
            feature_count,
        ))
    }

    fn new(name: String, bounds: [f64; 4], data: VectorData, numeric_properties: Vec<NumericProperty>, points_only: bool) -> Self {
        let range = numeric_properties.first().map(NumericProperty::range);
        Self {
            id: NEXT_SOURCE_ID.fetch_add(1, Ordering::Relaxed),
            style_id: NEXT_STYLE_ID.fetch_add(1, Ordering::Relaxed),
            name,
            bounds,
            data,
            points_only,
            property: range.map(|_| 0),
            numeric_properties: numeric_properties.into(),
            palette: ColorMapPreset::Terrain,
            range,
        }
    }

    /// Retained point-only classification, computed once during loading.
    /// Materialized layers must be nonempty with valid, nonempty Point/MultiPoint
    /// geometries. Tiled layers trust only declared GDAL geometry metadata;
    /// unknown or mixed types are false, without scanning features or tiles.
    pub fn points_only(&self) -> bool {
        self.points_only
    }

    /// Numeric metadata prepared during loading, never by scanning on the UI thread.
    pub fn numeric_properties(&self) -> &[NumericProperty] {
        &self.numeric_properties
    }

    /// The selected numeric attribute, or `None` for uniform theme coloring.
    pub fn property(&self) -> Option<&str> {
        self.property.map(|ix| self.numeric_properties[ix].name())
    }

    /// The active palette; defaults to Terrain.
    pub fn palette(&self) -> ColorMapPreset {
        self.palette
    }

    /// The editable coloring range for the selected attribute.
    pub fn range(&self) -> Option<(f64, f64)> {
        self.range
    }

    /// The selected attribute's default range, independent of edits to `range()`.
    /// Constants are expanded and tiled sources use the unknown-range `(0, 1)`.
    pub fn data_range(&self) -> Option<(f64, f64)> {
        self.property.map(|ix| self.numeric_properties[ix].range())
    }

    /// A process-local style revision. No-op styling retains it, while changed
    /// styling gets a fresh identity without changing the layer's `id()`.
    pub fn style_id(&self) -> u64 {
        self.style_id
    }

    /// Selects a numeric attribute, retaining the palette and resetting its range.
    /// Returns an error for unknown or non-numeric attributes; shares loaded data.
    pub fn with_property(&self, property: &str) -> Result<Arc<Self>> {
        let ix = self
            .numeric_properties
            .iter()
            .position(|candidate| candidate.name() == property)
            .with_context(|| format!("Layer {:?} has no numeric property {property:?}", self.name))?;
        let mut source = self.clone();
        source.property = Some(ix);
        source.range = Some(self.numeric_properties[ix].range());
        if source.property != self.property || source.range != self.range {
            source.style_id = NEXT_STYLE_ID.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Arc::new(source))
    }

    /// Recolors the selected numeric attribute without reloading or copying data.
    /// Requires a selection and finite, strictly increasing bounds.
    pub fn restyled(&self, palette: ColorMapPreset, range: (f64, f64)) -> Result<Arc<Self>> {
        ensure!(self.property.is_some(), "Layer {:?} has no numeric property to color", self.name);
        ensure!(
            range.0.is_finite() && range.1.is_finite() && range.0 < range.1,
            "Color range must be finite with minimum < maximum"
        );
        let mut source = self.clone();
        source.palette = palette;
        source.range = Some(range);
        if source.palette != self.palette || source.range != self.range {
            source.style_id = NEXT_STYLE_ID.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Arc::new(source))
    }

    /// A process-local identity, independent of styling and raster-source identities.
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

    /// Borrowed WGS84 GeoJSON for direct native parsing, without cloning the data.
    pub fn geojson(&self) -> Option<&str> {
        match &self.data {
            VectorData::GeoJson(data) => Some(data.as_str()),
            VectorData::Tiled { .. } => None,
        }
    }

    /// Native PMTiles/MBTiles URL; materialized layers return `None`.
    pub fn tile_url(&self) -> Option<&str> {
        match &self.data {
            VectorData::Tiled { url } => Some(url),
            VectorData::GeoJson(_) => None,
        }
    }

    #[cfg(test)]
    pub fn source_json(&self) -> Value {
        match &self.data {
            VectorData::Tiled { url } => json!({ "type": "vector", "url": url }),
            VectorData::GeoJson(data) => json!({ "type": "geojson", "data": serde_json::from_str::<Value>(data).unwrap() }),
        }
    }

    /// Fill, line, and circle layers referencing the fixed source `local-vector`.
    ///
    /// Layer IDs include this source's identity; the caller must install the source
    /// under `local-vector` (or rewrite source references to display several at once).
    /// MapLibre's geometry-type filter also matches corresponding multi-geometries.
    /// The caller supplies a CSS fallback color resolved from the application theme.
    /// Missing, null and non-numeric values use that color; numeric values use the
    /// selected palette and clamp to its endpoint colors outside the display range.
    pub fn layers_json(&self, opacity: f32, color: &str) -> Vec<Value> {
        let opacity = if opacity.is_finite() { opacity.clamp(0.0, 1.0) } else { 1.0 };
        let color = self.color_expression(color);
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

    fn color_expression(&self, fallback: &str) -> Value {
        let Some((property, (min, max))) = self.property().zip(self.range) else {
            return json!(fallback);
        };
        // Clamp before subtracting to avoid overflow for out-of-range values.
        // Wide ranges also need scaled subtraction to keep the span finite.
        let value = json!(["max", min, ["min", max, ["number", ["get", property]]]]);
        let normalized = if (max - min).is_finite() {
            json!(["/", ["-", value, min], max - min])
        } else {
            json!(["/", ["-", ["/", value, 2.0], min / 2.0], max / 2.0 - min / 2.0])
        };
        let palette = ProcessedColorMap::<PALETTE_STOPS>::create_for_preset(self.palette, ColorMapDirection::Regular);
        let mut interpolation = vec![json!("interpolate"), json!(["linear"]), normalized];
        for ix in 0..PALETTE_STOPS {
            let t = ix as f64 / (PALETTE_STOPS - 1) as f64;
            let color = palette.get_color_by_value(ix as u8);
            interpolation.push(json!(t));
            interpolation.push(json!(format!("rgb({},{},{})", color.r, color.g, color.b)));
        }
        // `number` is an assertion, not a conversion: the lazy case branch keeps
        // strings (even numeric-looking strings), booleans and null out of it.
        json!(["case", ["==", ["typeof", ["get", property]], "number"], interpolation, fallback])
    }
}

fn geojson_points_only(features: &[Value]) -> bool {
    fn valid_position(value: &Value) -> bool {
        value
            .as_array()
            .is_some_and(|coordinates| coordinates.len() >= 2 && coordinates.iter().all(|coordinate| coordinate.as_f64().is_some_and(f64::is_finite)))
    }

    !features.is_empty()
        && features.iter().all(|feature| {
            let geometry = &feature["geometry"];
            match geometry["type"].as_str() {
                Some("Point") => valid_position(&geometry["coordinates"]),
                Some("MultiPoint") => geometry["coordinates"]
                    .as_array()
                    .is_some_and(|positions| !positions.is_empty() && positions.iter().all(valid_position)),
                _ => false,
            }
        })
}

fn schema_points_only(defn: &gdal::vector::Defn) -> bool {
    use gdal::vector::{OGRwkbGeometryType, geometry_type_flatten};

    defn.geom_fields().count() == 1
        && matches!(
            geometry_type_flatten(defn.geometry_type()),
            OGRwkbGeometryType::wkbPoint | OGRwkbGeometryType::wkbMultiPoint
        )
}

fn geojson_numeric_properties(features: &[Value]) -> Vec<NumericProperty> {
    // A mixed attribute qualifies if at least one value is finite and numeric.
    // Ignore other values, which retain the theme fallback at render time.
    let mut ranges: BTreeMap<&str, (f64, f64)> = BTreeMap::new();
    for feature in features {
        let Some(properties) = feature["properties"].as_object() else {
            continue;
        };
        for (name, value) in properties {
            let Some(value) = value.as_f64().filter(|value| value.is_finite()) else {
                continue;
            };
            ranges
                .entry(name)
                .and_modify(|(min, max)| {
                    *min = min.min(value);
                    *max = max.max(value);
                })
                .or_insert((value, value));
        }
    }
    ranges
        .into_iter()
        .map(|(name, (min, max))| {
            let range = if min < max {
                (min, max)
            } else {
                let padding = (min.abs() * 0.01).max(1.0);
                ((min - padding).max(-f64::MAX), (max + padding).min(f64::MAX))
            };
            NumericProperty { name: name.to_owned(), range }
        })
        .collect()
}

fn schema_numeric_properties(defn: &gdal::vector::Defn) -> Vec<NumericProperty> {
    use gdal::vector::OGRFieldType;

    defn.fields()
        .enumerate()
        .filter_map(|(ix, field)| {
            if !matches!(
                field.field_type(),
                OGRFieldType::OFTInteger | OGRFieldType::OFTInteger64 | OGRFieldType::OFTReal
            ) {
                return None;
            }
            // gdal 0.19 does not expose field subtypes. Query only the schema: OGR
            // represents booleans as integers with OSFTBoolean (1), not numeric data.
            // SAFETY: ix comes from this live definition's field iterator. GDAL's
            // field count and indices are c_int; both handles remain borrowed here.
            let is_boolean = unsafe { OGR_Fld_GetSubType(OGR_FD_GetFieldDefn(defn.c_defn(), ix as c_int)) == 1 };
            (!is_boolean).then(|| NumericProperty {
                name: field.name(),
                range: (0.0, 1.0),
            })
        })
        .collect()
}

fn bounds_array(bounds: geo::LatLonBounds) -> [f64; 4] {
    [bounds.west(), bounds.south(), bounds.east(), bounds.north()]
}

fn open_csv_dataset(path: &Path) -> Result<Dataset> {
    // Let GDAL parse quoting, delimiters and coordinate columns. Do not
    // infer attribute types: identifiers such as "00123" must survive.
    gdalio::dataset::open_read_only_with_options(
        path,
        Some(&[
            "X_POSSIBLE_NAMES=lon,long,longitude",
            "Y_POSSIBLE_NAMES=lat,latitude",
            "KEEP_GEOM_COLUMNS=YES",
            "AUTODETECT_TYPE=NO",
        ]),
    )
    .with_context(|| format!("Could not open CSV coordinates in {}", path.display()))
}

fn prepare_csv_layer(layer: &mut impl gdal::vector::LayerAccess, path: &Path) -> Result<Vec<String>> {
    let name = layer.name();
    let fields: Vec<String> = layer.defn().fields().map(|field| field.name()).collect();
    let has_alias = |aliases: &[&str]| aliases.iter().any(|alias| fields.iter().any(|name| name.eq_ignore_ascii_case(alias)));
    ensure!(
        has_alias(&["lat", "latitude"]) && has_alias(&["lon", "long", "longitude"]) && layer.defn().geom_fields().count() == 1,
        "CSV layer {name:?} requires latitude (lat or latitude) and longitude (lon, long or longitude) columns"
    );
    let projection = if path.is_dir() {
        path.join(&name).with_extension("prj")
    } else {
        path.with_extension("prj")
    };
    ensure!(
        layer.spatial_ref().is_some() || !projection.try_exists()?,
        "CSV layer {name:?} has an invalid coordinate reference system in {}",
        projection.display()
    );
    validate_csv_records(layer, &fields)?;
    Ok(fields)
}

fn valid_csv_point(geometry: &Value) -> bool {
    geometry["type"] == "Point"
        && geometry["coordinates"].as_array().is_some_and(|coordinates| {
            coordinates.len() >= 2
                && coordinates[0]
                    .as_f64()
                    .zip(coordinates[1].as_f64())
                    .is_some_and(|(lon, lat)| geo::Coordinate::latlon(lat, lon).is_valid() && (-180.0..=180.0).contains(&lon))
        })
}

fn restore_csv_properties(features: &mut [Value], fields: &[String], name: &str) -> Result<bool> {
    let mut changed = false;
    for (record_ix, feature) in features.iter_mut().enumerate() {
        ensure!(
            valid_csv_point(&feature["geometry"]),
            "CSV layer {name:?}, record {} has missing or invalid longitude/latitude coordinates",
            record_ix + 1
        );
        // GDAL leaves empty typed CSV cells unset; retain their
        // columns explicitly rather than dropping the properties.
        let properties = feature["properties"].as_object_mut().context("CSV feature properties must be an object")?;
        for field in fields {
            if let serde_json::map::Entry::Vacant(entry) = properties.entry(field) {
                entry.insert(Value::Null);
                changed = true;
            }
        }
    }
    Ok(changed)
}

fn validate_csv_records(layer: &mut impl gdal::vector::LayerAccess, fields: &[String]) -> Result<()> {
    let coordinate_field = |aliases: &[&str]| -> Result<usize> {
        let matches: Vec<usize> = fields
            .iter()
            .enumerate()
            .filter_map(|(ix, name)| aliases.iter().any(|alias| name.eq_ignore_ascii_case(alias)).then_some(ix))
            .collect();
        ensure!(
            matches.len() == 1,
            "CSV coordinate columns are ambiguous; keep one latitude and one longitude column"
        );
        Ok(matches[0])
    };
    let longitude = coordinate_field(&["lon", "long", "longitude"])?;
    let latitude = coordinate_field(&["lat", "latitude"])?;
    // SAFETY: Reset and inspect only this thread's GDAL error state. Check each
    // record before another GDAL call can erase a lossy-conversion warning.
    unsafe { CPLErrorReset() };
    for (record_ix, feature) in layer.features().enumerate() {
        // SAFETY: Reads only this thread's GDAL error state.
        ensure!(
            unsafe { CPLGetLastErrorType() } < 2,
            "CSV record {} has invalid coordinates or properties: {}",
            record_ix + 1,
            geo::gdalinterop::last_error_message()
        );
        let coordinates = feature.field_as_double(longitude)?.zip(feature.field_as_double(latitude)?);
        ensure!(
            coordinates.is_some_and(|(lon, lat)| lon.is_finite() && lat.is_finite()),
            "CSV record {} has missing or invalid coordinates",
            record_ix + 1
        );
        ensure!(
            feature.geometry_by_index(0).ok().is_some_and(|geometry| {
                if geometry.geometry_name() != "POINT" || geometry.is_empty() {
                    return false;
                }
                let (x, y, _) = geometry.get_point(0);
                coordinates == Some((x, y))
            }),
            "CSV record {} has conflicting geometry or invalid coordinates; geometry must use the longitude/latitude columns",
            record_ix + 1
        );
    }
    // SAFETY: A failed read must not be mistaken for the end of the CSV.
    ensure!(
        unsafe { CPLGetLastErrorType() } < 2,
        "Could not read CSV coordinates or properties: {}",
        geo::gdalinterop::last_error_message()
    );
    Ok(())
}

// geo owns translation and handle lifetimes. These thread-local error checks
// additionally detect read failures when GDAL returns a non-null partial output.
unsafe extern "C" {
    fn OGR_FD_GetFieldDefn(definition: *mut c_void, index: c_int) -> *mut c_void;
    fn OGR_Fld_GetSubType(field: *mut c_void) -> c_int;
    fn CPLErrorReset();
    fn CPLGetLastErrorType() -> c_int;
    fn CPLGetThreadLocalConfigOption(key: *const c_char, default: *const c_char) -> *const c_char;
    fn CPLSetThreadLocalConfigOption(key: *const c_char, value: *const c_char);
}

struct ScopedArrowApi {
    previous: Option<CString>,
    // Restoring another thread's TLS would leak the override on this thread.
    _not_send: PhantomData<Rc<()>>,
}

impl ScopedArrowApi {
    fn current() -> Option<CString> {
        // SAFETY: The key is NUL-terminated. GDAL returns either null or a valid
        // thread-local C string; copy it before the next configuration mutation.
        let value = unsafe { CPLGetThreadLocalConfigOption(OGR2OGR_ARROW_API.as_ptr(), std::ptr::null()) };
        if value.is_null() {
            None
        } else {
            // SAFETY: The non-null GDAL string remains valid until TLS is mutated.
            Some(unsafe { CStr::from_ptr(value) }.to_owned())
        }
    }

    fn set(value: Option<&CStr>) -> Self {
        let guard = Self {
            previous: Self::current(),
            _not_send: PhantomData,
        };
        // SAFETY: GDAL copies these C strings into the calling thread's config.
        // A null value removes only the TLS entry, not global config or the env.
        unsafe {
            CPLSetThreadLocalConfigOption(OGR2OGR_ARROW_API.as_ptr(), value.map_or(std::ptr::null(), CStr::as_ptr));
        }
        guard
    }
}

impl Drop for ScopedArrowApi {
    fn drop(&mut self) {
        // SAFETY: The saved value is owned, including a possible empty string.
        // The !Send guard guarantees restoration occurs on the original thread.
        unsafe {
            CPLSetThreadLocalConfigOption(
                OGR2OGR_ARROW_API.as_ptr(),
                self.previous.as_ref().map_or(std::ptr::null(), |value| value.as_ptr()),
            );
        }
    }
}

fn conversion_path() -> std::path::PathBuf {
    let id = NEXT_CONVERSION_ID.fetch_add(1, Ordering::Relaxed);
    format!("/vsimem/map-vector-{}-{id}.geojson", std::process::id()).into()
}

fn translate_dataset_layer(
    dataset: &Dataset,
    name: &str,
    index: usize,
    assign_wgs84: bool,
    is_csv: bool,
    progress: &mut Option<ConversionProgress<'_>>,
) -> Result<(String, Option<geo::LatLonBounds>)> {
    let layer_count = dataset.layer_count();
    let per_feature = progress.as_ref().is_some_and(|progress| progress.per_feature);
    if per_feature
        && index > 0
        && let Some(progress) = progress.as_mut()
    {
        (progress.callback)(VectorLoadProgress::Converting(index as f64 / layer_count as f64));
    }
    let report_progress = progress.is_some();
    let mut layer_progress = |event| {
        let event = match event {
            VectorLoadProgress::Converting(fraction) if fraction.is_finite() => {
                VectorLoadProgress::Converting((index as f64 + fraction.clamp(0.0, 1.0)) / layer_count as f64)
            }
            VectorLoadProgress::Converting(_) => return,
            VectorLoadProgress::Preparing => VectorLoadProgress::Preparing,
        };
        if let Some(progress) = progress.as_mut() {
            (progress.callback)(event);
        }
    };
    translate_layer(
        dataset,
        name,
        &conversion_path(),
        assign_wgs84,
        is_csv,
        report_progress.then_some(ConversionProgress {
            callback: &mut layer_progress,
            per_feature,
        }),
    )
}

fn translate_layer(
    dataset: &Dataset,
    layer: &str,
    output: &Path,
    assign_wgs84: bool,
    is_csv: bool,
    progress: Option<ConversionProgress<'_>>,
) -> Result<(String, Option<geo::LatLonBounds>)> {
    // The GeoJSON driver requires a nonexistent destination. Let translation
    // create it, and unlink only after all dataset handles close, even on error.
    let _cleanup = gdal::vsi::MemFileRef::new(output);
    let mut output_dataset = translate_geojson_dataset(dataset, layer, output, assign_wgs84, progress)?;
    // CSV warnings include lossy field conversions, which must not silently
    // discard properties. CE_Warning = 2, CE_Failure = 3, CE_Fatal = 4.
    // SAFETY: Reads only this thread's GDAL error state.
    ensure!(
        unsafe { CPLGetLastErrorType() } < if is_csv { 2 } else { 3 },
        "GDAL vector conversion failed: {}",
        geo::gdalinterop::last_error_message()
    );
    let closed = Instant::now();
    output_dataset.flush_cache()?;
    // Closing completes the GeoJSON FeatureCollection before it is read.
    output_dataset.close()?;
    log::info!(target: "map::vector_load", "layer={layer:?} stage=finish-geojson elapsed={:?}", closed.elapsed());
    read_converted_geojson(output, layer)
}

fn geojson_translation_arguments(layer: &str, assign_wgs84: bool) -> Vec<String> {
    let mut arguments = vec![
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
    if assign_wgs84 {
        // CSV lat/lon headers establish WGS84 even without a CRS sidecar.
        arguments.extend(["-s_srs", "OGC:CRS84"]);
    }
    arguments.into_iter().map(str::to_owned).collect()
}

fn translate_geojson_dataset(
    dataset: &Dataset,
    layer: &str,
    output: &Path,
    assign_wgs84: bool,
    mut progress: Option<ConversionProgress<'_>>,
) -> Result<Dataset> {
    let arguments = geojson_translation_arguments(layer, assign_wgs84);
    // SAFETY: Resets only this thread's GDAL error state. A read failure must not
    // masquerade as end-of-layer even if GDAL returns a non-null output handle.
    unsafe { CPLErrorReset() };
    // Despite its name, geo's translator also accepts GDAL virtual paths.
    let translated = Instant::now();
    let per_feature = progress.as_ref().is_some_and(|progress| progress.per_feature);
    let report_progress = progress.is_some();
    let mut preparing = false;
    let output_dataset = {
        // Arrow can report only a terminal batch for small datasets. Preserve
        // the default/opt-in Arrow path for legacy conversion diagnostics.
        let _arrow_api = per_feature.then(|| ScopedArrowApi::set(Some(c"NO")));
        let mut callback = |fraction| {
            if let Some(progress) = progress.as_mut() {
                progress.report_translation(fraction, &mut preparing);
            }
        };
        algo::translate_ds_to_disk_with_progress(dataset, output, &arguments, report_progress.then_some(&mut callback as &mut dyn FnMut(f64)))?
    };
    // Some drivers disable progress when fast feature counts are unavailable.
    if per_feature
        && !preparing
        && let Some(progress) = progress.as_mut()
    {
        (progress.callback)(VectorLoadProgress::Preparing);
    }
    log::info!(target: "map::vector_load", "layer={layer:?} stage=gdal-convert-reproject elapsed={:?}", translated.elapsed());
    Ok(output_dataset)
}

fn read_converted_geojson(output: &Path, layer: &str) -> Result<(String, Option<geo::LatLonBounds>)> {
    let reopened = Instant::now();
    let converted = gdalio::dataset::open_read_only(output)?;
    log::info!(target: "map::vector_load", "layer={layer:?} stage=gdal-reopen-geojson elapsed={:?}", reopened.elapsed());
    let extent_started = Instant::now();
    // Only materialized vectors take this scan path, never tiled archives.
    let bounds = gdalio::layer_bounds_with_scan(&converted.layer(0)?).with_context(|| format!("Invalid converted coordinates in layer {layer:?}"))?;
    log::info!(target: "map::vector_load", "layer={layer:?} stage=gdal-extent elapsed={:?}", extent_started.elapsed());
    let closed = Instant::now();
    converted.close()?;
    log::info!(target: "map::vector_load", "layer={layer:?} stage=release-gdal-geojson elapsed={:?}", closed.elapsed());
    let copied = Instant::now();
    let geojson = String::from_utf8(gdal::vsi::get_vsi_mem_file_bytes_owned(output)?)?;
    log::info!(target: "map::vector_load", "layer={layer:?} stage=copy-geojson-buffer elapsed={:?} bytes={}", copied.elapsed(), geojson.len());
    Ok((geojson, bounds))
}

#[cfg(test)]
mod color_tests;
#[cfg(test)]
mod csv_tests;
#[cfg(test)]
mod format_tests;
#[cfg(test)]
mod geometry_tests;
#[cfg(test)]
mod progress_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn write_mbtiles(path: &Path) -> Result<()> {
        let connection = sqlite::Connection::new(path, sqlite::AccessMode::Create)?;
        connection.execute("CREATE TABLE metadata (name TEXT, value TEXT)")?;
        connection.execute("CREATE TABLE tiles (zoom_level INTEGER, tile_column INTEGER, tile_row INTEGER, tile_data BLOB)")?;
        let tile = mbtiles_fixture_tile();
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

    fn mbtiles_fixture_tile() -> Vec<u8> {
        let mut tile = Vec::new();
        for name in ["points", "polygons"] {
            let mut layer = vec![0x0a, name.len() as u8];
            layer.extend_from_slice(name.as_bytes());
            layer.extend_from_slice(&[0x28, 0x80, 0x20, 0x78, 0x02]);
            tile.extend_from_slice(&[0x1a, layer.len() as u8]);
            tile.extend(layer);
        }
        tile
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
            assert_native_mbtiles_source(&source, &path)?;
        }
        Ok(())
    }

    fn assert_native_mbtiles_source(source: &VectorSource, path: &Path) -> Result<()> {
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
    fn translation_returns_owned_geojson_and_releases_memory_on_success_and_failure() -> Result<()> {
        use gdal::spatial_ref::SpatialRef;
        use gdal::vector::{Geometry, LayerOptions};

        let mut dataset = gdalio::dataset::create_in_memory()?;
        let srs = SpatialRef::from_epsg(4326)?;
        let mut layer = dataset.create_layer(LayerOptions {
            name: "points",
            srs: Some(&srs),
            ..Default::default()
        })?;
        layer.create_feature(Geometry::from_wkt("POINT (4 51)")?)?;
        let output = conversion_path();
        let (json, bounds) = translate_layer(&dataset, "points", &output, false, false, None)?;
        assert_eq!(bounds.map(bounds_array), Some([4.0, 51.0, 4.0, 51.0]));
        assert_eq!(
            serde_json::from_str::<Value>(&json)?["features"][0]["geometry"]["coordinates"],
            json!([4.0, 51.0])
        );
        assert!(
            gdal::vsi::call_on_mem_file_bytes(&output, |_| ()).is_err(),
            "Successful translation left a VSI buffer allocated"
        );

        let output = conversion_path();
        assert!(translate_layer(&dataset, "missing-layer", &output, false, false, None).is_err());
        assert!(
            gdal::vsi::call_on_mem_file_bytes(&output, |_| ()).is_err(),
            "Failed translation left a VSI buffer allocated"
        );
        Ok(())
    }

    #[test]
    fn concurrent_in_memory_datasets_keep_independent_geojson_and_properties() -> Result<()> {
        let threads: Vec<_> = (0..8)
            .map(|index| {
                std::thread::spawn(move || -> Result<Arc<VectorSource>> {
                    let path = conversion_path();
                    let mut bytes = serde_json::to_vec(&json!({"type": "FeatureCollection", "features": [{
                        "type": "Feature", "geometry": {"type": "Point", "coordinates": [index, 51]},
                        "properties": {"value": index as f64 + 0.125, "name": format!("dataset-{index}")}
                    }]}))?;
                    let _input = gdal::vsi::create_mem_file_from_ref(&path, &mut bytes)?;
                    let source = VectorSource::open(&path)?.context("Expected in-memory GeoJSON")?.remove(0);
                    Ok(source)
                })
            })
            .collect();
        for (index, thread) in threads.into_iter().enumerate() {
            let source = thread.join().expect("Vector loading thread panicked")?;
            let data: Value = serde_json::from_str(source.geojson().context("Expected shared GeoJSON text")?)?;
            assert_eq!(
                data["features"][0]["properties"],
                json!({"value": index as f64 + 0.125, "name": format!("dataset-{index}")})
            );
            assert_eq!(source.bounds(), [index as f64, 51.0, index as f64, 51.0]);
            assert!(source.tile_url().is_none());
        }
        Ok(())
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

    fn write_point_layers(path: &Path) -> Result<()> {
        use gdal::spatial_ref::SpatialRef;
        use gdal::vector::{Geometry, LayerOptions};

        let mut dataset = gdal::DriverManager::get_driver_by_name("GPKG")?.create_vector_only(path)?;
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
        Ok(())
    }

    #[test]
    fn open_loads_every_layer_with_distinct_identities() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("layers.gpkg");
        write_point_layers(&path)?;
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
}
