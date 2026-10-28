use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock, Weak};
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, bail, ensure};
use gdal::Metadata as _;
use geo::{AnyDenseArray, Array, RasterScale, Tile};
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
    source_files: Option<Arc<tempfile::TempDir>>,
    unscaling_enabled: bool,
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
        let RasterMetadata {
            layers: metadata,
            source_files,
        } = raster_metadata(path, &options)?;
        ensure!(!metadata.is_empty(), "No raster bands found in {}", path.display());
        metadata
            .into_iter()
            .map(|metadata| Self::from_metadata(metadata, source_files.clone()))
            .collect()
    }

    fn from_metadata(mut metadata: LayerMetadata, source_files: Option<Arc<tempfile::TempDir>>) -> Result<Arc<Self>> {
        prepare_raster_metadata(&mut metadata)?;
        refresh_band_metadata(&mut metadata)?;
        validate_raster_scaling(&metadata)?;
        let unscaling_enabled = metadata.scale.is_some();
        let range = scaled_range(&metadata, metadata.scale.unwrap_or_default());
        Self::styled(Arc::new(metadata), source_files, ColorMapPreset::Turbo, range, unscaling_enabled)
    }

    fn styled(
        metadata: Arc<LayerMetadata>,
        source_files: Option<Arc<tempfile::TempDir>>,
        palette: ColorMapPreset,
        range: (f32, f32),
        unscaling_enabled: bool,
    ) -> Result<Arc<Self>> {
        ensure!(valid_range(range), "Minimum must be less than maximum; both must be finite numbers.");
        let legend = Legend::linear(&ColorMap::Preset(palette, ColorMapDirection::Regular), range.0..range.1, None)?;
        let (errors, error_receiver) = async_channel::bounded(1);
        Ok(Arc::new(Self {
            id: NEXT_STYLE_ID.fetch_add(1, Ordering::Relaxed),
            metadata,
            source_files,
            unscaling_enabled,
            legend,
            palette,
            range,
            errors,
            error_receiver,
        }))
    }

    pub fn restyled(&self, palette: ColorMapPreset, range: (f32, f32)) -> Result<Arc<Self>> {
        Self::styled(self.metadata.clone(), self.source_files.clone(), palette, range, self.unscaling_enabled)
    }

    /// Whether GDAL declares scale or offset, including explicit identity values.
    pub fn has_scale_metadata(&self) -> bool {
        self.metadata.scale.is_some()
    }

    pub fn unscaling_enabled(&self) -> bool {
        self.unscaling_enabled
    }

    /// Effective raw-to-display conversion; declared tiler metadata stays unchanged.
    pub fn scale(&self) -> RasterScale {
        if self.unscaling_enabled {
            self.metadata.scale.unwrap_or_default()
        } else {
            RasterScale::default()
        }
    }

    /// A memory-only style snapshot in the target units, sharing the raw data identity.
    /// Non-invertible or unrepresentable custom ranges reset to the target data range.
    pub fn with_unscaling(&self, enabled: bool) -> Result<Arc<Self>> {
        let enabled = enabled && self.has_scale_metadata();
        let target = if enabled {
            self.metadata.scale.unwrap_or_default()
        } else {
            RasterScale::default()
        };
        let range = if enabled == self.unscaling_enabled {
            self.range
        } else {
            self.converted_range(target)
        };
        Self::styled(self.metadata.clone(), self.source_files.clone(), self.palette, range, enabled)
    }

    fn converted_range(&self, target: RasterScale) -> (f32, f32) {
        let current = self.scale();
        let convert = |value: f32| ((f64::from(value) - current.offset) / current.scale * target.scale + target.offset) as f32;
        let first = convert(self.range.0);
        let last = convert(self.range.1);
        let converted = (first.min(last), first.max(last));
        if current.scale != 0.0 && valid_range(converted) {
            converted
        } else {
            scaled_range(&self.metadata, target)
        }
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
        scaled_range(&self.metadata, self.scale())
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
    /// NaN represents nodata. Scale and offset remain metadata for the shader;
    /// cached numeric values are never unscaled, normalized, or clipped.
    pub fn numeric_tile(&self, tile: Tile, dpi: u8) -> Result<Option<NumericTile>> {
        let metadata = &self.metadata;
        let request = TileRequest {
            tile,
            dpi_ratio: dpi,
            tile_size: metadata.tile_size.unwrap_or(TILE_SIZE),
            tile_format: TileFormat::RasterTile,
        };
        let raw = read_numeric_tile(metadata, &request)?;
        if raw.data.is_empty() {
            return Ok(None);
        }
        NumericTile::decode(&raw.data).map(Some)
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

impl NumericTile {
    fn decode(bytes: &[u8]) -> Result<Self> {
        let data: AnyDenseArray = AnyDenseArray::from_raster_tile_bytes(bytes)?;
        let data = data.cast_to::<f32>();
        let width = data.columns().0 as u32;
        let height = data.rows().0 as u32;
        let values = data.iter_opt().map(|value| value.filter(|v| v.is_finite()).unwrap_or(f32::NAN)).collect();
        Ok(Self { width, height, values })
    }
}

struct RasterMetadata {
    layers: Vec<LayerMetadata>,
    source_files: Option<Arc<tempfile::TempDir>>,
}

fn raster_metadata(path: &Path, options: &TileProviderOptions) -> Result<RasterMetadata> {
    // Probing must not prevent formats handled by native readers (such as
    // raster MBTiles) from opening without their optional GDAL driver.
    let dataset = gdal::Dataset::open(path);
    if dataset.as_ref().is_ok_and(|dataset| dataset.driver().short_name() == "netCDF") {
        let (metadata, directory) = netcdf_metadata(path, options)?;
        return Ok(RasterMetadata {
            layers: metadata,
            source_files: Some(directory),
        });
    }
    let mut metadata = provider_metadata(path, options)?;
    if metadata.iter().any(|meta| meta.source_format == LayerSourceType::CloudOptimizedGeoTiff) && dataset?.raster_count() > 1 {
        metadata = WarpingTileProvider::new(path, options)?.layers();
        for meta in &mut metadata {
            meta.source_format = LayerSourceType::GeoTiff;
        }
    }
    Ok(RasterMetadata {
        layers: metadata,
        source_files: None,
    })
}

fn provider_metadata(path: &Path, options: &TileProviderOptions) -> Result<Vec<LayerMetadata>> {
    // The optimized COG reader only accepts web-tile-aligned COGs. Other
    // COGs and GDAL formats (including VRT/ASCII) use on-demand warping.
    DynamicTileProvider::new(options.clone())
        .add_file(path)
        .or_else(|_| WarpingTileProvider::new(path, options).map(|provider| provider.layers()))
        .with_context(|| format!("Could not open {}", path.display()))
}

fn prepare_raster_metadata(metadata: &mut LayerMetadata) -> Result<()> {
    if metadata.source_format == LayerSourceType::Unknown {
        // VRT and other GDAL-only drivers use the generic warping provider.
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
    Ok(())
}

fn refresh_band_metadata(metadata: &mut LayerMetadata) -> Result<()> {
    let Some(band_nr) = metadata.band_nr else { return Ok(()) };
    let dataset = gdal::Dataset::open(&metadata.path)?;
    let band = dataset.rasterband(band_nr)?;
    metadata.description = band.description()?.trim().to_owned();
    metadata.scale = match (band.scale(), band.offset()) {
        (None, None) => None,
        (scale, offset) => Some(RasterScale {
            scale: scale.unwrap_or(1.0),
            offset: offset.unwrap_or(0.0),
        }),
    };
    // Force calculation when no statistics were stored; otherwise the range
    // can be 0..f32::MAX, including in debug builds.
    if let Some(stats) = band.get_statistics(true, true)? {
        metadata.min_value = stats.min as f32;
        metadata.max_value = stats.max as f32;
    }
    Ok(())
}

fn validate_raster_scaling(metadata: &LayerMetadata) -> Result<()> {
    let RasterScale { scale, offset } = metadata.scale.unwrap_or_default();
    ensure!(
        scale.is_finite() && offset.is_finite() && (scale as f32).is_finite() && (offset as f32).is_finite() && (scale == 0.0 || scale as f32 != 0.0),
        "Raster scale and offset must be finite and representable by the renderer"
    );
    Ok(())
}

fn read_numeric_tile(metadata: &LayerMetadata, request: &TileRequest) -> tiler::Result<tiler::TileData> {
    let warp = || {
        let mut metadata = metadata.clone();
        metadata.min_zoom = 0;
        WarpingTileProvider::tile(&metadata, request)
    };
    if metadata.source_format != LayerSourceType::CloudOptimizedGeoTiff {
        return DynamicTileProvider::get_tile_for_layer(metadata, request);
    }
    if request.tile.z < metadata.min_zoom {
        warp()
    } else {
        DynamicTileProvider::get_tile_for_layer(metadata, request).or_else(|_| warp())
    }
}

/// GDAL exposes spatial NetCDF variables as subdatasets and flattens extra
/// dimensions into bands. VRTs retain that mapping without copying grid data.
fn netcdf_metadata(path: &Path, options: &TileProviderOptions) -> Result<(Vec<LayerMetadata>, Arc<tempfile::TempDir>)> {
    // VRTs live in a different directory; their sources must be absolute paths.
    let absolute = path.canonicalize()?;
    let dataset = gdal::Dataset::open(&absolute)?;
    let grids = netcdf_subdatasets(&dataset, &absolute);
    let directory = Arc::new(tempfile::tempdir().context("Could not create NetCDF raster directory")?);
    let name = path.file_stem().context("NetCDF file has no name")?.to_string_lossy();
    let mut metadata = Vec::new();
    let mut errors = Vec::new();
    let mut auxiliary_variables = HashSet::new();
    let candidates = netcdf_candidates(grids, &mut auxiliary_variables, &mut errors);
    for candidate in candidates {
        if netcdf_is_auxiliary(&candidate, &auxiliary_variables)? {
            continue;
        }
        let output = directory.path().join(format!("grid-{}.vrt", candidate.index));
        match netcdf_grid_metadata(&candidate.dataset, &absolute, &output, &name, options) {
            Ok(layers) => metadata.extend(layers),
            Err(error) => {
                log::warn!("Skipping NetCDF variable {}: {error:#}", candidate.source);
                errors.push(format!("{}: {error:#}", candidate.source));
            }
        }
    }
    ensure!(
        !metadata.is_empty(),
        "No georeferenced 2D grids found in {}: {}",
        path.display(),
        errors.join("; ")
    );
    Ok((metadata, directory))
}

struct NetcdfCandidate {
    index: usize,
    source: String,
    dataset: gdal::Dataset,
    variable: String,
}

fn netcdf_subdatasets(dataset: &gdal::Dataset, absolute: &Path) -> Vec<(usize, String)> {
    let mut grids = dataset
        .metadata_domain("SUBDATASETS")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            let index = key.strip_prefix("SUBDATASET_")?.strip_suffix("_NAME")?.parse::<usize>().ok()?;
            Some((index, value.to_owned()))
        })
        .collect::<Vec<_>>();
    grids.sort_by_key(|(index, _)| *index);
    if grids.is_empty() {
        grids.push((0, absolute.to_string_lossy().into_owned()));
    }
    grids
}

fn netcdf_candidates(grids: Vec<(usize, String)>, auxiliary_variables: &mut HashSet<String>, errors: &mut Vec<String>) -> Vec<NetcdfCandidate> {
    let mut candidates = Vec::new();
    for (index, grid) in grids {
        match open_netcdf_grid(&grid).and_then(|dataset| {
            let variable = netcdf_variable_name(&dataset)?;
            Ok((dataset, variable))
        }) {
            Ok((dataset, variable)) => {
                netcdf_collect_auxiliary_variables(&dataset, &variable, auxiliary_variables);
                candidates.push(NetcdfCandidate {
                    index,
                    source: grid,
                    dataset,
                    variable,
                });
            }
            Err(error) => errors.push(format!("{grid}: {error:#}")),
        }
    }
    candidates
}

fn netcdf_collect_auxiliary_variables(dataset: &gdal::Dataset, variable: &str, auxiliary_variables: &mut HashSet<String>) {
    // In multi-variable files, CF relationships are exposed on the
    // subdatasets, not necessarily on the container's metadata.
    for entry in dataset.metadata_domain("").unwrap_or_default() {
        if let Some((key, value)) = entry.split_once('=')
            && key
                .rsplit_once('#')
                .is_some_and(|(_, attribute)| matches!(attribute, "coordinates" | "bounds" | "climatology"))
        {
            auxiliary_variables.extend(value.split_whitespace().map(|reference| netcdf_reference(variable, reference)));
        }
    }
}

fn netcdf_is_auxiliary(candidate: &NetcdfCandidate, auxiliary_variables: &HashSet<String>) -> gdal::errors::Result<bool> {
    let band = candidate.dataset.rasterband(1)?;
    Ok(auxiliary_variables.contains(candidate.variable.trim_start_matches('/'))
        || band.metadata_item("standard_name", "").is_some_and(|name| {
            matches!(
                name.as_str(),
                "latitude" | "longitude" | "projection_x_coordinate" | "projection_y_coordinate" | "time"
            )
        })
        || band
            .metadata_item("axis", "")
            .is_some_and(|axis| matches!(axis.as_str(), "X" | "Y" | "Z" | "T")))
}

fn open_netcdf_grid(grid: &str) -> gdal::errors::Result<gdal::Dataset> {
    let dataset = gdal::Dataset::open(grid)?;
    if dataset.geo_transform().is_err() {
        // Some producers omit CF axis attributes on otherwise valid X/Y
        // coordinates. Let GDAL derive the transform and matching row order
        // from those coordinates instead of assuming the attribute's order.
        if let Ok(coordinates) = gdal::Dataset::open_ex(
            grid,
            gdal::DatasetOptions {
                open_options: Some(&["IGNORE_XY_AXIS_NAME_CHECKS=YES"]),
                ..Default::default()
            },
        ) && coordinates.geo_transform().is_ok()
        {
            return Ok(coordinates);
        }
    }
    Ok(dataset)
}

fn netcdf_grid_metadata(dataset: &gdal::Dataset, source: &Path, output: &Path, file_name: &str, options: &TileProviderOptions) -> Result<Vec<LayerMetadata>> {
    ensure!(dataset.raster_count() > 0, "No raster bands found");
    let first_band = dataset.rasterband(1)?;
    let mut vrt = dataset.create_copy(&gdal::DriverManager::get_driver_by_name("VRT")?, output, &Default::default())?;
    netcdf_georeference_vrt(dataset, &first_band, &mut vrt, source, output)?;
    vrt.close()?;
    let mut metadata = WarpingTileProvider::new(output, options)?.layers();
    netcdf_label_layers(dataset, file_name, &mut metadata)?;
    Ok(metadata)
}

fn netcdf_georeference_vrt(
    dataset: &gdal::Dataset,
    first_band: &gdal::raster::RasterBand<'_>,
    vrt: &mut gdal::Dataset,
    source: &Path,
    output: &Path,
) -> Result<()> {
    if dataset.geo_transform().is_err() {
        // Some producers put GeoTransform on the data variable instead of the
        // grid-mapping variable, where GDAL expects it. Never guess an extent.
        let transform = netcdf_explicit_transform(first_band)?;
        // The classic reader may reverse rows even when it cannot derive a
        // transform. The explicit attribute describes native array order, so
        // use lazy multidimensional sources rather than that reversed reader.
        netcdf_native_sources(dataset, vrt, source, output)?;
        vrt.set_geo_transform(&transform)?;
    }
    if dataset.spatial_ref().is_err() {
        let crs = dataset
            .metadata_item("NC_GLOBAL#crs", "")
            .context("No coordinate reference system or global crs attribute")?;
        vrt.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_definition(&crs)?)?;
    }
    Ok(())
}

fn netcdf_explicit_transform(band: &gdal::raster::RasterBand<'_>) -> Result<[f64; 6]> {
    let attribute = band
        .metadata_item("GeoTransform", "")
        .context("No affine transform or GeoTransform attribute")?;
    let values = attribute
        .split_whitespace()
        .map(str::parse::<f64>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let transform: [f64; 6] = values.try_into().map_err(|_| anyhow::anyhow!("GeoTransform must contain six numbers"))?;
    ensure!(
        transform.iter().all(|value| value.is_finite()) && (transform[1] * transform[5] - transform[2] * transform[4]) != 0.0,
        "GeoTransform must be finite and invertible"
    );
    Ok(transform)
}

fn netcdf_label_layers(dataset: &gdal::Dataset, file_name: &str, metadata: &mut [LayerMetadata]) -> Result<()> {
    for meta in metadata {
        let band_nr = meta.band_nr.context("NetCDF raster has no band number")?;
        let band = dataset.rasterband(band_nr)?;
        let variable = netcdf_variable_name(dataset)?;
        let mut dimensions = band
            .metadata_domain("")
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| entry.strip_prefix("NETCDF_DIM_").map(str::to_owned))
            .collect::<Vec<_>>();
        dimensions.sort();
        meta.name = format!("{file_name} - {variable}");
        if !dimensions.is_empty() {
            meta.name.push_str(&format!(" [{}]", dimensions.join(", ")));
        } else if dataset.raster_count() > 1 {
            meta.name.push_str(&format!(" - Band {band_nr}"));
        }
        meta.source_format = LayerSourceType::Netcdf;
    }
    Ok(())
}

fn netcdf_native_sources(dataset: &gdal::Dataset, vrt: &mut gdal::Dataset, source: &Path, output: &Path) -> Result<()> {
    let (variable, sizes) = netcdf_native_dimensions(source, dataset)?;
    netcdf_validate_native_dimensions(dataset, &sizes)?;
    let native = netcdf_native_vrt(dataset, source, output, &variable, &sizes)?;
    std::fs::write(&native.path, native.xml)?;
    for (index, source_xml) in native.sources.iter().enumerate() {
        vrt.rasterband(index + 1)?.set_metadata_item("source_0", source_xml, "vrt_sources")?;
    }
    Ok(())
}

fn netcdf_native_dimensions(source: &Path, dataset: &gdal::Dataset) -> Result<(String, Vec<usize>)> {
    let multidimensional = gdal::Dataset::open_ex(
        source,
        gdal::DatasetOptions {
            open_flags: gdal::GdalOpenFlags::GDAL_OF_MULTIDIM_RASTER,
            allowed_drivers: Some(&["netCDF"]),
            ..Default::default()
        },
    )?;
    let mut group = multidimensional.root_group()?;
    let variable = netcdf_variable_name(dataset)?;
    let (groups, name) = variable.rsplit_once('/').unwrap_or(("", &variable));
    for name in groups.split('/').filter(|name| !name.is_empty()) {
        group = group.open_group(name, Default::default())?;
    }
    let array = group.open_md_array(name, Default::default())?;
    let dimensions = array.dimensions()?;
    let sizes = dimensions.iter().map(|dimension| dimension.size()).collect();
    Ok((variable, sizes))
}

fn netcdf_validate_native_dimensions(dataset: &gdal::Dataset, sizes: &[usize]) -> Result<()> {
    ensure!(sizes.len() >= 2, "NetCDF variable has fewer than two dimensions");
    let extra = sizes.len() - 2;
    ensure!(
        dataset.raster_size() == (sizes[extra + 1], sizes[extra]) && dataset.raster_count() == sizes[..extra].iter().product::<usize>(),
        "NetCDF native dimensions do not match its raster bands"
    );
    Ok(())
}

struct NetcdfNativeVrt {
    path: std::path::PathBuf,
    xml: String,
    sources: Vec<String>,
}

fn netcdf_native_vrt(dataset: &gdal::Dataset, source: &Path, output: &Path, variable: &str, sizes: &[usize]) -> Result<NetcdfNativeVrt> {
    let filename = escape_xml(source.to_str().context("NetCDF path is not valid UTF-8")?);
    let array_name = escape_xml(&format!("/{}", variable.trim_start_matches('/')));
    let native_path = output.with_extension("native.vrt");
    let native_filename = escape_xml(native_path.to_str().context("NetCDF VRT path is not valid UTF-8")?);
    let (width, height) = dataset.raster_size();
    let mut native_xml = format!("<VRTDataset rasterXSize=\"{width}\" rasterYSize=\"{height}\">");
    let mut sources = Vec::new();
    for band_nr in 1..=dataset.raster_count() {
        let data_type = dataset.rasterband(band_nr)?.band_type().name();
        native_xml.push_str(&netcdf_native_band_xml(&filename, &array_name, sizes, band_nr, &data_type));
        sources.push(format!(
            "<ComplexSource><SourceFilename relativeToVRT=\"0\">{native_filename}</SourceFilename>\
             <SourceBand>{band_nr}</SourceBand><UseMaskBand>true</UseMaskBand></ComplexSource>"
        ));
    }
    native_xml.push_str("</VRTDataset>");
    Ok(NetcdfNativeVrt {
        path: native_path,
        xml: native_xml,
        sources,
    })
}

fn netcdf_native_band_xml(filename: &str, array_name: &str, sizes: &[usize], band_nr: usize, data_type: &str) -> String {
    let mut index = band_nr - 1;
    let mut view = vec![":".to_owned(); sizes.len()];
    for dimension in (0..sizes.len() - 2).rev() {
        view[dimension] = (index % sizes[dimension]).to_string();
        index /= sizes[dimension];
    }
    let view = view.join(",");
    let array_source = |mask: &str| {
        format!(
            "<ArraySource><DerivedArray><SingleSourceArray>\
             <SourceFilename relativeToVRT=\"0\">{filename}</SourceFilename><SourceArray>{array_name}</SourceArray>\
             </SingleSourceArray>{mask}<Step><View expr=\"[{view}]\"/></Step></DerivedArray></ArraySource>"
        )
    };
    // Native reads bypass the classic driver's CF validity checks. Apply
    // GetMask before slicing so its attributes still describe the raw array.
    format!(
        "<VRTRasterBand dataType=\"{data_type}\" band=\"{band_nr}\">{}\
         <MaskBand><VRTRasterBand dataType=\"Byte\">{}</VRTRasterBand></MaskBand></VRTRasterBand>",
        array_source(""),
        array_source("<Step><GetMask/></Step>")
    )
}

fn netcdf_variable_name(dataset: &gdal::Dataset) -> gdal::errors::Result<String> {
    let description = dataset.description()?;
    if let Some((_, variable)) = description.strip_prefix("NETCDF:").and_then(|description| description.rsplit_once(':')) {
        return Ok(variable.trim_matches('"').to_owned());
    }
    let variable = dataset.rasterband(1)?.metadata_item("NETCDF_VARNAME", "").unwrap_or_default();
    // A single-variable container has no subdataset description. Its dataset
    // attribute keys retain the group path that NETCDF_VARNAME omits.
    Ok(dataset
        .metadata_domain("")
        .unwrap_or_default()
        .into_iter()
        .find_map(|entry| {
            let (key, _) = entry.split_once('=')?;
            let (name, _) = key.rsplit_once('#')?;
            (name.rsplit('/').next() == Some(variable.as_str())).then(|| name.to_owned())
        })
        .unwrap_or(variable))
}

fn netcdf_reference(variable: &str, reference: &str) -> String {
    if !reference.starts_with('/')
        && let Some((group, _)) = variable.rsplit_once('/')
    {
        format!("{group}/{reference}").trim_start_matches('/').to_owned()
    } else {
        reference.trim_start_matches('/').to_owned()
    }
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn scaled_range(metadata: &LayerMetadata, RasterScale { scale, offset }: RasterScale) -> (f32, f32) {
    let first = (f64::from(metadata.min_value) * scale + offset) as f32;
    let last = (f64::from(metadata.max_value) * scale + offset) as f32;
    display_range(first.min(last), first.max(last))
}

fn valid_range((min, max): (f32, f32)) -> bool {
    min.is_finite() && max.is_finite() && (max - min).is_finite() && min < max
}

fn display_range(min: f32, max: f32) -> (f32, f32) {
    if valid_range((min, max)) {
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
            std::thread::Builder::new()
                .name(format!("raster-tiles-{worker}"))
                .spawn(move || run_tile_worker(receiver, sources))?;
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

fn run_tile_worker(receiver: async_channel::Receiver<TileJob>, sources: Sources) {
    while let Ok(job) = receiver.recv_blocking() {
        if job.cancelled.load(Ordering::Relaxed) {
            continue;
        }
        let response = tile_response(&sources, &job.url);
        job.responder.complete(response);
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
    #[ignore = "requires MAP_TEST_NETCDF pointing to a local NetCDF file"]
    fn netcdf_local_file_opens_all_layers_and_reads_tiles() -> Result<()> {
        let path = std::path::PathBuf::from(std::env::var("MAP_TEST_NETCDF")?);
        assert!(crate::vector_source::VectorSource::open(&path)?.is_none());
        print_netcdf_coordinates(&path)?;
        let sources = RasterSource::open(&path)?;
        eprintln!("Opened {} NetCDF raster layers", sources.len());
        for source in sources {
            let metadata = source.metadata();
            let [west, south, east, north] = metadata.bounds;
            let coordinate = geo::Coordinate::latlon((south + north) / 2.0, (west + east) / 2.0);
            let tile = Tile::for_coordinate(coordinate, metadata.max_zoom.clamp(0, 22));
            let data = source.numeric_tile(tile, 1)?.context("Missing NetCDF tile")?;
            assert_eq!(data.values.len(), data.width as usize * data.height as usize);
            eprintln!("{}: bounds={:?}, range={:?}", metadata.name, metadata.bounds, source.range());
        }
        Ok(())
    }

    fn print_netcdf_coordinates(path: &Path) -> Result<()> {
        let multidimensional = gdal::Dataset::open_ex(
            path,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_MULTIDIM_RASTER,
                ..Default::default()
            },
        )?;
        let group = multidimensional.root_group()?;
        for name in group.array_names(Default::default()) {
            let array = group.open_md_array(&name, Default::default())?;
            let dimensions = array.dimensions()?;
            if dimensions.len() == 1 && dimensions[0].size() > 1 && array.datatype().class().is_numeric() {
                print_netcdf_coordinate_endpoints(&name, &array, dimensions[0].size())?;
            }
        }
        Ok(())
    }

    fn print_netcdf_coordinate_endpoints(name: &str, array: &gdal::raster::MDArray<'_>, size: usize) -> Result<()> {
        let first = array.read_as::<f64>(vec![0], vec![1])?[0];
        let last = array.read_as::<f64>(vec![size as u64 - 1], vec![1])?[0];
        eprintln!("Coordinate {name}: first={first}, last={last}");
        Ok(())
    }

    #[test]
    fn netcdf_coordinate_references_resolve_relative_to_their_variable_group() {
        assert_eq!(netcdf_reference("/grid", "latitude"), "latitude");
        assert_eq!(netcdf_reference("/group/grid", "latitude"), "group/latitude");
        assert_eq!(netcdf_reference("/group/grid", "/latitude"), "latitude");
    }

    #[test]
    fn netcdf_rejects_grids_without_georeferencing_instead_of_guessing_an_extent() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("unreferenced.nc");
        let dataset = gdal::DriverManager::get_driver_by_name("netCDF")?.create_with_band_type::<f32, _>(&path, 4, 4, 1)?;
        dataset.close()?;
        let error = RasterSource::open(&path).err().context("An unreferenced grid should be rejected")?;
        assert!(format!("{error:#}").contains("No georeferenced 2D grids"));
        Ok(())
    }

    #[test]
    fn netcdf_adds_each_grid_variable_without_coordinate_variables() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("variables.nc");
        super::test_support::write_netcdf(&path, false, false)?;
        assert!(crate::vector_source::VectorSource::open(&path)?.is_none());
        let subdatasets = gdal::Dataset::open(&path)?
            .metadata_domain("SUBDATASETS")
            .context("Missing NetCDF subdatasets")?;
        assert!(
            subdatasets.iter().any(|entry| entry.ends_with(":lat")),
            "Fixture must contain a 2D latitude coordinate array"
        );
        assert!(
            subdatasets.iter().any(|entry| entry.ends_with(":lon")),
            "Fixture must contain a 2D longitude coordinate array"
        );
        let sources = RasterSource::open(&path)?;
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].metadata().name, "variables - Band1");
        assert_eq!(sources[1].metadata().name, "variables - Band2");
        assert_ne!(sources[0].metadata().id, sources[1].metadata().id);
        for (index, source) in sources.iter().enumerate() {
            let tile = source.numeric_tile(Tile { z: 3, x: 3, y: 3 }, 1)?.context("Missing NetCDF tile")?;
            assert!(tile.values.iter().any(|value| (*value - (index + 1) as f32 * 10.0).abs() <= 1.0));
        }
        Ok(())
    }

    #[test]
    fn netcdf_time_slices_use_dimension_labels_and_preserve_temporary_sources_when_restyled() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("time.nc");
        super::test_support::write_netcdf(&path, true, false)?;
        let sources = RasterSource::open(&path)?;
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].metadata().name, "time - WBGT [time=0]");
        assert_eq!(sources[1].metadata().name, "time - WBGT [time=1]");
        let restyled = sources[1].restyled(ColorMapPreset::Gray, (0.0, 30.0))?;
        let toggled = restyled.with_unscaling(false)?;
        assert!(Arc::ptr_eq(sources[1].source_files.as_ref().unwrap(), toggled.source_files.as_ref().unwrap()));
        let vrt_path = toggled.metadata().path.clone();
        drop(restyled);
        let restyled = toggled;
        drop(sources);
        let tile = restyled.numeric_tile(Tile { z: 3, x: 3, y: 3 }, 1)?.context("Missing NetCDF time slice")?;
        assert!(tile.values.iter().any(|value| (20.0..=21.0).contains(value)));
        drop(restyled);
        assert!(
            !vrt_path.exists(),
            "Temporary sources should be removed after their last style snapshot is dropped"
        );
        Ok(())
    }

    #[test]
    fn netcdf_uses_explicit_transform_and_crs_attributes_when_gdal_cannot_find_georeferencing() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("metadata & transform.nc");
        super::test_support::write_netcdf(&path, true, true)?;
        let original = gdal::Dataset::open(&path)?;
        assert!(original.geo_transform().is_err());
        let source = RasterSource::open(&path)?.remove(0);
        assert_eq!(source.metadata().epsg, Some(geo::crs::epsg::WGS84_WEB_MERCATOR));
        assert_eq!(
            read_raster_values(&source.metadata().path, 1, (0, 0), (1, 1))?.data(),
            &[10.0],
            "Northern rows must remain north"
        );
        assert_eq!(
            read_raster_values(&source.metadata().path, 1, (0, 63), (1, 1))?.data(),
            &[11.0],
            "Southern rows must remain south"
        );
        let tile = source
            .numeric_tile(Tile { z: 3, x: 3, y: 3 }, 1)?
            .context("Missing georeferenced NetCDF tile")?;
        assert!(tile.values.contains(&10.0));
        assert!(gdal::Dataset::open(&path)?.geo_transform().is_err(), "The original file must not be modified");
        Ok(())
    }

    #[test]
    fn netcdf_grouped_native_fallback_resolves_container_and_subdataset_paths() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("grouped.nc");
        write_netcdf_native_fixture(&path, true, "", None)?;
        let dataset = gdal::Dataset::open(&path)?;
        assert!(dataset.geo_transform().is_err());
        assert_eq!(netcdf_variable_name(&dataset)?, "/forecast/day/WBGT");
        let sources = RasterSource::open(&path)?;
        assert_eq!(sources.len(), 2);
        assert_grouped_netcdf_values(&sources)?;
        let selected = gdal::Dataset::open(format!("NETCDF:\"{}\":/forecast/day/WBGT", path.display()))?;
        let output = dir.path().join("selected.vrt");
        let metadata = netcdf_grid_metadata(&selected, &path, &output, "grouped", &TileProviderOptions::default())?;
        assert_eq!(metadata.len(), 2);
        assert!(metadata[0].name.starts_with("grouped - /forecast/day/WBGT"));
        assert_eq!(read_raster_values(&output, 2, (0, 0), (1, 2))?.data(), &[20.0, 21.0]);
        Ok(())
    }

    fn read_raster_values(path: &Path, band: usize, origin: (isize, isize), size: (usize, usize)) -> Result<gdal::raster::Buffer<f32>> {
        let dataset = gdal::Dataset::open(path)?;
        Ok(dataset.rasterband(band)?.read_as::<f32>(origin, size, size, None)?)
    }

    fn assert_grouped_netcdf_values(sources: &[Arc<RasterSource>]) -> Result<()> {
        for (index, source) in sources.iter().enumerate() {
            let values = read_raster_values(&source.metadata().path, index + 1, (0, 0), (4, 2))?;
            let value = (index + 1) as f32 * 10.0;
            assert_eq!(values.data(), &[value, -1.0, 1000.0, -9999.0, value + 1.0, 0.0, 100.0, value + 2.0]);
        }
        Ok(())
    }

    #[test]
    fn raster_rejects_scaling_that_cannot_be_represented_by_shader_uniforms() -> Result<()> {
        let dir = tempfile::tempdir()?;
        for (index, (scale, offset)) in [(1e-46, 0.0), (f64::MAX, 0.0), (1.0, f64::MAX)].into_iter().enumerate() {
            let path = dir.path().join(format!("invalid-scale-{index}.tif"));
            super::test_support::write_raster(&path, 4326, 1)?;
            let dataset = gdal::Dataset::open_ex(
                &path,
                gdal::DatasetOptions {
                    open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE | gdal::GdalOpenFlags::GDAL_OF_RASTER,
                    ..Default::default()
                },
            )?;
            let mut band = dataset.rasterband(1)?;
            band.set_scale(scale)?;
            band.set_offset(offset)?;
            dataset.close()?;
            let error = RasterSource::open(&path)
                .err()
                .context("Unrepresentable scaling must not silently render incorrectly")?;
            assert!(error.to_string().contains("Raster scale and offset"));
        }
        Ok(())
    }

    fn open_scaled_fixture(directory: &Path, scale: Option<f64>, offset: Option<f64>) -> Result<Arc<RasterSource>> {
        let path = directory.join("scaled.tif");
        super::test_support::write_raster(&path, 4326, 1)?;
        if scale.is_some() || offset.is_some() {
            // GeoTIFF can omit explicitly set identity coefficients on save.
            // PAM preserves the declaration so presence is tested independently of value.
            let scale = scale.map_or(String::new(), |value| format!("<Scale>{value}</Scale>"));
            let offset = offset.map_or(String::new(), |value| format!("<Offset>{value}</Offset>"));
            std::fs::write(
                directory.join("scaled.tif.aux.xml"),
                format!("<PAMDataset><PAMRasterBand band=\"1\">{scale}{offset}</PAMRasterBand></PAMDataset>"),
            )?;
        }
        Ok(RasterSource::open(&path)?.remove(0))
    }

    #[test]
    fn unscaling_metadata_presence_distinguishes_absent_identity_and_offset_only() -> Result<()> {
        for (scale, offset) in [(None, None), (Some(1.0), None), (None, Some(0.0)), (Some(1.0), Some(0.0)), (None, Some(7.0))] {
            let dir = tempfile::tempdir()?;
            let source = open_scaled_fixture(dir.path(), scale, offset)?;
            let present = scale.is_some() || offset.is_some();
            assert_eq!(source.has_scale_metadata(), present, "Declared scale {scale:?}, offset {offset:?}");
            assert_eq!(source.unscaling_enabled(), present);
            assert_eq!(
                source.scale(),
                RasterScale {
                    scale: scale.unwrap_or(1.0),
                    offset: offset.unwrap_or(0.0)
                }
            );
            let raw = source.with_unscaling(false)?;
            assert_eq!(raw.has_scale_metadata(), present);
            assert!(!raw.unscaling_enabled());
            assert_eq!(raw.scale(), RasterScale::default());
            assert_eq!(raw.with_unscaling(true)?.unscaling_enabled(), present);
        }
        Ok(())
    }

    #[test]
    fn unscaling_converts_custom_ranges_in_both_directions_and_sorts_negative_scale() -> Result<()> {
        for (scale, physical) in [(2.0, (50.0, 90.0)), (-2.0, (-70.0, -30.0))] {
            let dir = tempfile::tempdir()?;
            let source = open_scaled_fixture(dir.path(), Some(scale), Some(10.0))?.restyled(ColorMapPreset::Gray, physical)?;
            let raw = source.with_unscaling(false)?;
            assert_eq!(raw.range(), (20.0, 40.0));
            assert_eq!(raw.data_range(), (source.metadata().min_value, source.metadata().max_value));
            assert_eq!(raw.legend().total_value_range(), 20.0..=40.0);
            let physical_again = raw.with_unscaling(true)?;
            assert_eq!(physical_again.range(), physical);
            assert_eq!(physical_again.data_range(), source.data_range());
            assert_eq!(physical_again.legend().total_value_range(), physical.0..=physical.1);
            assert_eq!(physical_again.with_unscaling(true)?.range(), physical);
            assert_eq!(raw.with_unscaling(false)?.range(), raw.range());
        }
        Ok(())
    }

    #[test]
    fn unscaling_zero_scale_resets_noninvertible_and_collapsed_ranges_to_target_data() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = open_scaled_fixture(dir.path(), Some(0.0), Some(12.0))?.restyled(ColorMapPreset::Blues, (9.0, 15.0))?;
        let raw = source.with_unscaling(false)?;
        assert_eq!(raw.range(), raw.data_range());
        assert_eq!(raw.data_range(), (source.metadata().min_value, source.metadata().max_value));
        let custom_raw = raw.restyled(ColorMapPreset::Gray, (20.0, 40.0))?;
        let physical = custom_raw.with_unscaling(true)?;
        assert_eq!(physical.scale(), RasterScale { scale: 0.0, offset: 12.0 });
        assert_eq!(physical.range(), (11.0, 13.0));
        assert_eq!(physical.range(), physical.data_range());
        assert_eq!(physical.legend().total_value_range(), 11.0..=13.0);
        Ok(())
    }

    #[test]
    fn unscaling_invalid_converted_ranges_reset_to_target_data() -> Result<()> {
        for (scale, offset, enabled, range) in [
            (1e-30, 0.0, false, (1e20, 2e20)),
            (1e30, 0.0, true, (1e20, 2e20)),
            (2.0, 0.0, true, (-1e38, 1e38)),
            (1.0, 1e20, true, (20.0, 40.0)),
        ] {
            let dir = tempfile::tempdir()?;
            let source = open_scaled_fixture(dir.path(), Some(scale), Some(offset))?;
            let source = source.with_unscaling(!enabled)?.restyled(ColorMapPreset::Gray, range)?;
            let toggled = source.with_unscaling(enabled)?;
            assert_eq!(toggled.range(), toggled.data_range());
            assert!(valid_range(toggled.range()));
        }
        Ok(())
    }

    #[test]
    fn unscaling_snapshots_preserve_metadata_raw_tiles_palette_and_cache_identity_without_reads() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let source = open_scaled_fixture(dir.path(), Some(-2.0), Some(10.0))?.restyled(ColorMapPreset::Blues, (-70.0, -30.0))?;
        let raw = source.with_unscaling(false)?;
        let tile = Tile { z: 3, x: 3, y: 3 };
        let before = source.numeric_tile(tile, 1)?.context("Missing scaled tile")?;
        let after = raw.numeric_tile(tile, 1)?.context("Missing raw tile")?;
        assert_eq!(
            before.values.iter().map(|value| value.to_bits()).collect::<Vec<_>>(),
            after.values.iter().map(|value| value.to_bits()).collect::<Vec<_>>()
        );
        assert_unscaled_snapshots_without_reads(&source, &raw)
    }

    fn assert_unscaled_snapshots_without_reads(source: &RasterSource, raw: &RasterSource) -> Result<()> {
        // No subsequent snapshot operation may reopen the backing dataset.
        std::fs::remove_file(&source.metadata().path)?;
        let toggled = source.with_unscaling(false)?;
        let restyled = toggled.restyled(ColorMapPreset::Gray, (25.0, 30.0))?;
        for snapshot in [raw, &*toggled, &*restyled] {
            assert_ne!(source.id(), snapshot.id());
            assert!(Arc::ptr_eq(&source.metadata, &snapshot.metadata));
            assert_eq!(source.metadata().id, snapshot.metadata().id);
            assert_eq!(snapshot.metadata().scale, Some(RasterScale { scale: -2.0, offset: 10.0 }));
            assert_eq!(source.source_json(2), snapshot.source_json(2));
            assert!(snapshot.has_scale_metadata());
            assert!(!snapshot.unscaling_enabled());
        }
        assert_eq!(toggled.palette(), source.palette());
        assert_eq!(toggled.palette_rgba(), source.palette_rgba());
        assert_eq!(restyled.with_unscaling(true)?.range(), (-50.0, -40.0));
        Ok(())
    }

    #[test]
    fn raster_scale_metadata_changes_physical_ranges_but_keeps_tiles_raw() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("scaled.tif");
        super::test_support::write_raster(&path, 4326, 3)?;
        let original = RasterSource::open(&path)?;
        let coefficients = [(0.01, -4.0), (-0.5, 7.0), (0.0, 12.0)];
        set_band_scales(&path, &coefficients)?;
        let scaled = RasterSource::open(&path)?;
        for (index, (scale, offset)) in coefficients.iter().copied().enumerate() {
            assert_scaled_source(&original[index], &scaled[index], RasterScale { scale, offset })?;
        }
        Ok(())
    }

    fn set_band_scales(path: &Path, coefficients: &[(f64, f64)]) -> Result<()> {
        let dataset = gdal::Dataset::open_ex(
            path,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE | gdal::GdalOpenFlags::GDAL_OF_RASTER,
                ..Default::default()
            },
        )?;
        for (index, (scale, offset)) in coefficients.iter().copied().enumerate() {
            let mut band = dataset.rasterband(index + 1)?;
            band.set_scale(scale)?;
            band.set_offset(offset)?;
        }
        dataset.close()?;
        Ok(())
    }

    fn assert_scaled_source(original: &RasterSource, source: &RasterSource, RasterScale { scale, offset }: RasterScale) -> Result<()> {
        assert_eq!(source.metadata().scale, Some(RasterScale { scale, offset }));
        assert_eq!(source.metadata().min_value, original.metadata().min_value, "Statistics stay in raw units");
        assert_eq!(source.metadata().max_value, original.metadata().max_value);
        let first = (f64::from(source.metadata().min_value) * scale + offset) as f32;
        let last = (f64::from(source.metadata().max_value) * scale + offset) as f32;
        let expected = if scale == 0.0 { (11.0, 13.0) } else { (first.min(last), first.max(last)) };
        assert_eq!(source.data_range(), expected);
        assert_eq!(source.range(), expected);
        let tile = Tile { z: 3, x: 3, y: 3 };
        let raw = original.numeric_tile(tile, 1)?.context("Missing original tile")?;
        let values = source.numeric_tile(tile, 1)?.context("Missing scaled tile")?;
        assert!(
            raw.values.iter().zip(&values.values).all(|(a, b)| a == b || (a.is_nan() && b.is_nan())),
            "Scale/offset must not change cached pixel values or nodata"
        );
        let restyled = source.restyled(ColorMapPreset::Gray, expected)?;
        assert_eq!(restyled.metadata().scale, Some(RasterScale { scale, offset }));
        assert_eq!(restyled.metadata().id, source.metadata().id);
        Ok(())
    }

    #[test]
    fn netcdf_native_fallback_masks_cf_validity_and_keeps_raw_values_for_shader_unscaling() -> Result<()> {
        for validity in [
            "<Attribute name=\"valid_range\"><DataType>Float32</DataType><Value>0</Value><Value>100</Value></Attribute>",
            "<Attribute name=\"valid_min\"><DataType>Float32</DataType><Value>0</Value></Attribute>\
             <Attribute name=\"valid_max\"><DataType>Float32</DataType><Value>100</Value></Attribute>",
        ] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("validity & raw.nc");
            write_netcdf_native_fixture(&path, false, validity, None)?;
            let sources = RasterSource::open(&path)?;
            assert_eq!(sources.len(), 2);
            for (index, source) in sources.iter().enumerate() {
                let vrt = gdal::Dataset::open(&source.metadata().path)?;
                let band = vrt.rasterband(index + 1)?;
                assert_eq!(band.no_data_value(), Some(-9999.0));
                assert_eq!(band.scale(), Some(0.5));
                assert_eq!(band.offset(), Some(-7.0));
                let value = (index + 1) as f32 * 10.0;
                assert_eq!(
                    band.read_as::<f32>((0, 0), (4, 2), (4, 2), None)?.data(),
                    &[value, -9999.0, -9999.0, -9999.0, value + 1.0, 0.0, 100.0, value + 2.0]
                );
                assert_eq!(source.metadata().scale, Some(RasterScale { scale: 0.5, offset: -7.0 }));
                assert_eq!(source.data_range(), (-7.0, 43.0));
            }
        }
        Ok(())
    }

    #[test]
    fn netcdf_detects_ascending_and_descending_y_coordinates_without_cf_axis_attributes() -> Result<()> {
        for (coordinates, expected) in [([500_000.0, -500_000.0], [10.0, 11.0]), ([-500_000.0, 500_000.0], [11.0, 10.0])] {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join("orientation.nc");
            write_netcdf_native_fixture(&path, false, "", Some(coordinates))?;
            assert!(
                gdal::Dataset::open(&path)?.geo_transform().is_err(),
                "Fixture must reproduce unrecognized coordinate axes"
            );
            let sources = RasterSource::open(&path)?;
            assert_eq!(sources.len(), 2);
            assert_netcdf_orientation(&sources, expected)?;
        }
        Ok(())
    }

    fn assert_netcdf_orientation(sources: &[Arc<RasterSource>], expected: [f32; 2]) -> Result<()> {
        for (index, source) in sources.iter().enumerate() {
            let vrt = gdal::Dataset::open(&source.metadata().path)?;
            assert_eq!(vrt.geo_transform()?, [-1_000_000.0, 500_000.0, 0.0, 1_000_000.0, 0.0, -1_000_000.0]);
            let values = vrt.rasterband(index + 1)?.read_as::<f32>((0, 0), (1, 2), (1, 2), None)?;
            assert_eq!(
                values.data(),
                &expected.map(|value| value + index as f32 * 10.0),
                "Coordinates must determine north/south ordering"
            );
        }
        Ok(())
    }

    // Multidimensional VRT input lets GDAL create grouped NetCDF fixtures through
    // its safe CreateCopy API, without a netCDF dependency or raw native calls.
    fn write_netcdf_native_fixture(path: &Path, grouped: bool, validity: &str, y_coordinates: Option<[f64; 2]>) -> Result<()> {
        let coordinates = y_coordinates.map_or_else(String::new, |[first, last]| {
            format!(
                "<Array name=\"x\"><DataType>Float64</DataType><DimensionRef ref=\"x\"/>\
                 <InlineValues>-750000 -250000 250000 750000</InlineValues></Array>\
                 <Array name=\"y\"><DataType>Float64</DataType><DimensionRef ref=\"y\"/>\
                 <InlineValues>{first} {last}</InlineValues></Array>"
            )
        });
        let grid = format!(
            "<Dimension name=\"time\" size=\"2\"/><Dimension name=\"y\" size=\"2\"/><Dimension name=\"x\" size=\"4\"/>{coordinates}\
             <Array name=\"WBGT\"><DataType>Float32</DataType>\
             <DimensionRef ref=\"time\"/><DimensionRef ref=\"y\"/><DimensionRef ref=\"x\"/>\
             <Attribute name=\"GeoTransform\"><DataType>String</DataType>\
             <Value>-1000000 500000 0 1000000 0 -1000000</Value></Attribute>{validity}\
             <NoDataValue>-9999</NoDataValue><Scale>0.5</Scale><Offset>-7</Offset>\
             <InlineValues>10 -1 1000 -9999 11 0 100 12 20 -1 1000 -9999 21 0 100 22</InlineValues></Array>"
        );
        let grid = if grouped {
            format!("<Group name=\"forecast\"><Group name=\"day\">{grid}</Group></Group>")
        } else {
            grid
        };
        let fixture = path.with_extension("fixture.vrt");
        std::fs::write(
            &fixture,
            format!(
                "<VRTDataset><Group name=\"/\"><Attribute name=\"crs\"><DataType>String</DataType>\
                 <Value>EPSG:3857</Value></Attribute>{grid}</Group></VRTDataset>"
            ),
        )?;
        let dataset = gdal::Dataset::open_ex(
            &fixture,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_MULTIDIM_RASTER,
                ..Default::default()
            },
        )?;
        dataset
            .create_copy(
                &gdal::DriverManager::get_driver_by_name("netCDF")?,
                path,
                &gdal::raster::RasterCreationOptions::from_iter(["FORMAT=NC4"]),
            )?
            .close()?;
        Ok(())
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
        write_zero_raster_without_nodata(&path)?;

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

    fn write_zero_raster_without_nodata(path: &Path) -> Result<()> {
        let mut dataset = gdal::DriverManager::get_driver_by_name("GTiff")?.create_with_band_type::<f32, _>(path, 64, 64, 1)?;
        dataset.set_geo_transform(&[-1_000_000.0, 31_250.0, 0.0, 1_000_000.0, 0.0, -31_250.0])?;
        dataset.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(3857)?)?;
        dataset.rasterband(1)?.fill(0.0, None)?;
        dataset.flush_cache()?;
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
        assert!(palette.as_chunks::<4>().0.iter().all(|color| color[3] == 255));
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
        write_red_image_mbtiles(&path)?;
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

    fn write_red_image_mbtiles(path: &Path) -> Result<()> {
        // Build MBTiles directly: the static GDAL build need not enable its
        // optional MBTiles driver for our native SQLite tile provider to work.
        let db = sqlite::Connection::new(path, sqlite::AccessMode::Create)?;
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
        Ok(())
    }

    #[test]
    fn optimized_cogs_use_native_tiles_and_synthesize_missing_overviews() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("input.tif");
        super::test_support::write_raster(&path, 3857, 1)?;
        let cog_path = dir.path().join("aligned-cog.tif");
        write_aligned_cog(&path, &cog_path)?;
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

    fn write_aligned_cog(path: &Path, cog_path: &Path) -> Result<()> {
        let input = gdal::Dataset::open(path)?;
        let options = gdal::raster::RasterCreationOptions::from_iter([
            "TILING_SCHEME=GoogleMapsCompatible",
            "ZOOM_LEVEL=3",
            "BLOCKSIZE=256",
            "OVERVIEWS=NONE",
            "COMPRESS=DEFLATE",
            "ADD_ALPHA=NO",
        ]);
        let cog = input.create_copy(&gdal::DriverManager::get_driver_by_name("COG")?, cog_path, &options)?;
        drop(cog);
        // The tiler's COG probe reads a full 16-KiB header. Keep this tiny,
        // highly compressible fixture large enough to exercise that path.
        let file = std::fs::OpenOptions::new().write(true).open(cog_path)?;
        file.set_len(file.metadata()?.len().max(16 * 1024))?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub fn write_netcdf(path: &Path, time_dimension: bool, metadata_only_transform: bool) -> Result<()> {
        let mut dataset = gdal::DriverManager::get_driver_by_name("MEM")?.create_with_band_type::<f32, _>("", 64, 64, 2)?;
        configure_netcdf_fixture(&mut dataset, time_dimension, metadata_only_transform)?;
        for band_nr in 1..=2 {
            write_netcdf_band(&dataset, band_nr, time_dimension, metadata_only_transform)?;
        }
        dataset
            .create_copy(
                &gdal::DriverManager::get_driver_by_name("netCDF")?,
                path,
                &gdal::raster::RasterCreationOptions::from_iter(["FORMAT=NC", "WRITE_BOTTOMUP=NO", "WRITE_LONLAT=YES"]),
            )?
            .close()?;
        Ok(())
    }

    fn configure_netcdf_fixture(dataset: &mut gdal::Dataset, time_dimension: bool, metadata_only_transform: bool) -> Result<()> {
        if metadata_only_transform {
            dataset.set_metadata_item("NC_GLOBAL#crs", "EPSG:3857", "")?;
        } else {
            dataset.set_geo_transform(&[-1_000_000.0, 31_250.0, 0.0, 1_000_000.0, 0.0, -31_250.0])?;
            dataset.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(3857)?)?;
        }
        if time_dimension {
            for (name, value) in [
                ("NETCDF_DIM_EXTRA", "{time}"),
                ("NETCDF_DIM_time_DEF", "{2,6}"),
                ("NETCDF_DIM_time_VALUES", "{0,1}"),
                ("time#units", "hours since 2024-06-20 00:00:00"),
            ] {
                dataset.set_metadata_item(name, value, "")?;
            }
        }
        Ok(())
    }

    fn write_netcdf_band(dataset: &gdal::Dataset, band_nr: usize, time_dimension: bool, metadata_only_transform: bool) -> Result<()> {
        let mut band = dataset.rasterband(band_nr)?;
        band.set_no_data_value(Some(-9999.0))?;
        if time_dimension {
            band.set_metadata_item("NETCDF_VARNAME", "WBGT", "")?;
            band.set_metadata_item("NETCDF_DIM_time", &(band_nr - 1).to_string(), "")?;
        }
        if metadata_only_transform {
            band.set_metadata_item("GeoTransform", "-1000000 31250 0 1000000 0 -31250", "")?;
        }
        let values = (0..4096).map(|index| band_nr as f32 * 10.0 + if index / 64 < 32 { 0.0 } else { 1.0 }).collect();
        band.write((0, 0), (64, 64), &mut gdal::raster::Buffer::new((64, 64), values))?;
        Ok(())
    }

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
            write_raster_band(&dataset, band_nr)?;
        }
        dataset.flush_cache()?;
        Ok(())
    }

    fn write_raster_band(dataset: &gdal::Dataset, band_nr: usize) -> Result<()> {
        let mut band = dataset.rasterband(band_nr)?;
        band.set_no_data_value(Some(-9999.0))?;
        let values = (0..4096)
            .map(|ix| if ix % 64 < 8 { -9999.0 } else { (ix % 64 + ix / 64 + band_nr * 10) as f32 })
            .collect();
        band.write((0, 0), (64, 64), &mut gdal::raster::Buffer::new((64, 64), values))?;
        Ok(())
    }
}
