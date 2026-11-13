use std::collections::HashSet;

use super::*;

#[test]
fn grouped_coordinate_grids_use_declared_epsg_and_preserve_pixel_locations() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("copenhagen.nc");
    super::test_support::write_netcdf_coordinate_grids(&path, Some(32633))?;
    let original = std::fs::read(&path)?;
    let sources = RasterSource::open_with_epsg(&path, Some(4326))?;
    assert_eq!(sources.len(), 3, "Coordinate arrays must not appear as data layers");
    for source in &sources {
        assert_eq!(source.metadata().epsg, Some(geo::crs::Epsg::new(32633)));
        assert!(source.range().0.is_finite() && source.range().1.is_finite());
    }
    let source = &sources[0];
    assert!(source.metadata().name.contains("/Surface/T2M"));
    let dataset = gdal::Dataset::open(&source.metadata().path)?;
    assert!(std::fs::read_to_string(&source.metadata().path)?.contains("VRTWarpedDataset"));
    for row in 0..3 {
        for column in 0..4 {
            let x = 339699.0 + column as f64 * 200.0;
            let y = 6166080.0 + row as f64 * 200.0;
            assert_eq!(value_at_projected_coordinate(&dataset, 1, x, y)?, (10 + row * 4 + column) as f32);
        }
    }
    let [west, south, east, north] = source.metadata().bounds;
    assert!((12.0..13.0).contains(&west) && (55.0..56.0).contains(&south));
    assert!(west < east && south < north);
    assert_tile_contains_band_values(source)?;
    assert_eq!(std::fs::read(&path)?, original, "Opening must not modify the source file");
    Ok(())
}

fn value_at_projected_coordinate(dataset: &gdal::Dataset, band: usize, x: f64, y: f64) -> Result<f32> {
    use gdal::GeoTransformEx as _;
    let (column, row) = dataset.geo_transform()?.invert()?.apply(x, y);
    Ok(dataset
        .rasterband(band)?
        .read_as::<f32>((column.floor() as isize, row.floor() as isize), (1, 1), (1, 1), None)?
        .data()[0])
}

#[test]
fn cf_geolocation_discovered_by_gdal_needs_no_named_xy_fallback() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("cf.nc");
    write_modified_coordinate_grids(
        &path,
        &[
            ("<Array name=\"X\">", "<Array name=\"unused_easting\">"),
            ("<Array name=\"Y\">", "<Array name=\"unused_northing\">"),
            (
                "<Array name=\"T2M\">",
                "<Array name=\"T2M\"><Attribute name=\"coordinates\"><DataType>String</DataType><Value>/Parameters/LAT /Parameters/LON</Value></Attribute>",
            ),
            ("<Value>Degrees</Value>", "<Value>degrees_north</Value>"),
            (
                "<Array name=\"LON\"><DataType>Float32</DataType><DimensionRef ref=\"/nx\"/><DimensionRef ref=\"/ny\"/><Attribute name=\"units\"><DataType>String</DataType><Value>degrees_north</Value>",
                "<Array name=\"LON\"><DataType>Float32</DataType><DimensionRef ref=\"/nx\"/><DimensionRef ref=\"/ny\"/><Attribute name=\"units\"><DataType>String</DataType><Value>degrees_east</Value>",
            ),
            ("name=\"nx\"", "name=\"y\""),
            ("ref=\"/nx\"", "ref=\"/y\""),
            ("name=\"ny\"", "name=\"x\""),
            ("ref=\"/ny\"", "ref=\"/x\""),
        ],
    )?;
    let grid = gdal::Dataset::open(format!("NETCDF:\"{}\":/Surface/T2M", path.display()))?;
    assert!(grid.geo_transform().is_err());
    assert!(grid.metadata_item("X_BAND", "GEOLOCATION").is_some());
    let output = directory.path().join("cf.vrt");
    let layers = netcdf_grid_metadata(&grid, &path, &output, "cf", &TileProviderOptions::default(), None)?;
    assert_eq!(layers.len(), 1);
    let warped = gdal::Dataset::open(&layers[0].path)?;
    for row in 0..3 {
        for column in 0..4 {
            assert_eq!(
                value_at_projected_coordinate(&warped, 1, 12.0 + column as f64, 55.0 + row as f64)?,
                (10 + row * 4 + column) as f32
            );
        }
    }
    Ok(())
}

#[test]
fn grouped_coordinate_grids_with_non_affine_eastings_preserve_pixel_locations() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("curved.nc");
    write_modified_coordinate_grids(
        &path,
        &[(
            "339699 339899 340099 340299 339699 339899 340099 340299 339699 339899 340099 340299",
            "339699 339899 340099 340299 339739 339939 340139 340339 339699 339899 340099 340299",
        )],
    )?;
    let sources = RasterSource::open(&path)?;
    let source = sources
        .iter()
        .find(|source| source.metadata().name.contains("/Surface/T2M"))
        .context("Missing T2M layer")?;
    let dataset = gdal::Dataset::open(&source.metadata().path)?;
    assert_projected_grid_values(&dataset, [339699.0, 339739.0, 339699.0], [6166080.0, 6166280.0, 6166480.0])?;
    Ok(())
}

#[test]
fn grouped_coordinate_grids_with_descending_northings_preserve_pixel_locations() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("descending.nc");
    write_modified_coordinate_grids(
        &path,
        &[(
            "6166080 6166080 6166080 6166080 6166280 6166280 6166280 6166280 6166480 6166480 6166480 6166480",
            "6166480 6166480 6166480 6166480 6166280 6166280 6166280 6166280 6166080 6166080 6166080 6166080",
        )],
    )?;
    let sources = RasterSource::open(&path)?;
    let source = sources
        .iter()
        .find(|source| source.metadata().name.contains("/Surface/T2M"))
        .context("Missing T2M layer")?;
    let dataset = gdal::Dataset::open(&source.metadata().path)?;
    assert_projected_grid_values(&dataset, [339699.0; 3], [6166480.0, 6166280.0, 6166080.0])?;
    Ok(())
}

#[test]
fn grouped_coordinate_grids_with_mismatched_shapes_report_georeferencing_not_missing_projection() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("mismatched.nc");
    write_modified_coordinate_grids(
        &path,
        &[(
            "<DataType>Float32</DataType><DimensionRef ref=\"/nx\"/><DimensionRef ref=\"/ny\"/>",
            "<DataType>Float32</DataType><DimensionRef ref=\"/ny\"/><DimensionRef ref=\"/nx\"/>",
        )],
    )?;
    let error = RasterSource::open(&path).err().context("Mismatched coordinate dimensions should be rejected")?;
    assert!(
        !error.is::<MissingProjection>(),
        "A declared EPSG must not hide a coordinate shape error: {error:#}"
    );
    let message = format!("{error:#}");
    assert!(message.contains("No georeferenced 2D grids"), "Unexpected error: {message}");
    assert!(
        message.contains("spatial axes"),
        "The coordinate shape failure should remain actionable: {message}"
    );
    Ok(())
}

#[test]
fn geolocation_warp_preserves_raw_samples_scale_offset_and_nodata() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("scaled.nc");
    write_modified_coordinate_grids(
        &path,
        &[(
            "<Array name=\"T2M\">",
            "<Array name=\"T2M\"><Scale>0.5</Scale><Offset>-7</Offset><NoDataValue>21</NoDataValue>",
        )],
    )?;
    let sources = RasterSource::open(&path)?;
    let source = sources
        .iter()
        .find(|source| source.metadata().name.contains("/Surface/T2M"))
        .context("Missing T2M layer")?;
    assert!(source.unscaling_enabled());
    assert_eq!(source.range(), (-2.0, 3.0));
    let dataset = gdal::Dataset::open(&source.metadata().path)?;
    let band = dataset.rasterband(1)?;
    assert_eq!(band.scale(), Some(0.5));
    assert_eq!(band.offset(), Some(-7.0));
    assert_eq!(band.no_data_value(), Some(21.0));
    assert_eq!(value_at_projected_coordinate(&dataset, 1, 339699.0, 6166080.0)?, 10.0);
    assert_eq!(value_at_projected_coordinate(&dataset, 1, 340299.0, 6166480.0)?, 21.0);
    Ok(())
}

#[test]
fn grouped_coordinate_grids_restyled_sources_retain_lazy_geolocation_dependencies() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("restyled.nc");
    super::test_support::write_netcdf_coordinate_grids(&path, Some(32633))?;
    let sources = RasterSource::open(&path)?;
    let source = sources
        .iter()
        .find(|source| source.metadata().name.contains("/Surface/T2M"))
        .context("Missing T2M layer")?;
    let restyled = source.restyled(ColorMapPreset::Gray, (10.0, 21.0))?;
    let vrt_path = restyled.metadata().path.clone();
    drop(sources);
    let dataset = gdal::Dataset::open(&vrt_path)?;
    assert_projected_grid_values(&dataset, [339699.0; 3], [6166080.0, 6166280.0, 6166480.0])?;
    assert_tile_contains_band_values(&restyled)?;
    drop(dataset);
    drop(restyled);
    assert!(!vrt_path.exists(), "Lazy dependencies must be released with the last source snapshot");
    Ok(())
}

fn assert_projected_grid_values(dataset: &gdal::Dataset, eastings: [f64; 3], northings: [f64; 3]) -> Result<()> {
    for (row, (x, y)) in eastings.into_iter().zip(northings).enumerate() {
        for column in 0..4 {
            assert_eq!(
                value_at_projected_coordinate(dataset, 1, x + column as f64 * 200.0, y)?,
                (10 + row * 4 + column) as f32,
                "Wrong raw value at source row {row}, column {column}"
            );
        }
    }
    Ok(())
}

fn write_modified_coordinate_grids(path: &Path, replacements: &[(&str, &str)]) -> Result<()> {
    super::test_support::write_netcdf_coordinate_grids(path, Some(32633))?;
    let fixture = path.with_extension("fixture.vrt");
    let mut xml = std::fs::read_to_string(&fixture)?;
    for (original, replacement) in replacements {
        assert!(xml.contains(original), "Fixture XML no longer contains {original}");
        xml = xml.replace(original, replacement);
    }
    std::fs::write(&fixture, xml)?;
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
fn grouped_coordinate_grids_without_crs_request_projection_and_accept_supplied_epsg() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("copenhagen.nc");
    super::test_support::write_netcdf_coordinate_grids(&path, None)?;
    let error = RasterSource::open(&path).err().context("Expected a projection request")?;
    assert!(error.is::<MissingProjection>(), "Unexpected error: {error:#}");
    let sources = RasterSource::open_with_epsg(&path, Some(32633))?;
    assert_eq!(sources.len(), 3);
    assert_tile_contains_band_values(&sources[0])?;
    Ok(())
}

#[test]
fn supplied_epsg_3035_uses_easting_northing_order_for_plain_rasters() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("lisbon.tif");
    super::test_support::write_raster(&path, 0, 1)?;
    let mut dataset = gdal::Dataset::open_ex(
        &path,
        gdal::DatasetOptions {
            open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
            ..Default::default()
        },
    )?;
    dataset.set_geo_transform(&[2_635_554.0, 300.0, 0.0, 1_997_575.0, 0.0, -300.0])?;
    dataset.close()?;
    let source = RasterSource::open_with_epsg(&path, Some(3035))?.remove(0);
    let [west, south, _, _] = source.metadata().bounds;
    assert_tile_contains_band_values(&source)?;
    assert!((-10.0..-8.0).contains(&west) && (38.0..40.0).contains(&south));
    Ok(())
}

#[test]
fn projected_netcdf_time_series_retains_all_bands_and_cached_or_sampled_ranges() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("lisbon-series.nc");
    write_netcdf_series(&path, 128)?;
    assert!(
        RasterSource::open(&path)
            .err()
            .context("Expected missing projection")?
            .is::<MissingProjection>()
    );
    let sources = RasterSource::open_with_epsg(&path, Some(3035))?;
    assert_eq!(sources.len(), 128);
    assert_eq!(sources.iter().map(|source| source.metadata().id).collect::<HashSet<_>>().len(), sources.len());
    assert!(crate::raster_playback::PlaybackRequest::for_dataset(&sources).is_some());
    assert!(sources[0].metadata().name.contains("time=0"));
    assert!(sources[127].metadata().name.contains("time=127"));
    assert_eq!(sources[0].range(), (1.0, 1.5));
    assert_eq!(sources[127].range(), (128.0, 128.5));
    assert_eq!(sources[63].range(), (64.0, 64.5), "Cached per-band statistics must remain independent");
    assert_eq!(sources[64].range(), (1.0, 128.5), "Unscanned frames use the sampled time-series range");
    assert!(
        gdal::Dataset::open(&path)?.spatial_ref().is_err(),
        "The source CRS must not be written to the file"
    );
    Ok(())
}

#[test]
fn projected_netcdf_time_series_reads_both_endpoint_tiles_in_the_shared_geographic_extent() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("lisbon-series.nc");
    write_netcdf_series(&path, 128)?;
    let sources = RasterSource::open_with_epsg(&path, Some(3035))?;
    for source in [&sources[0], &sources[127]] {
        let metadata = source.metadata();
        assert_eq!(metadata.epsg, Some(geo::crs::Epsg::new(3035)));
        assert_eq!(metadata.bounds, sources[0].metadata().bounds);
        assert_eq!(metadata.path, sources[0].metadata().path);
        assert_tile_contains_band_values(source)?;
    }
    Ok(())
}

fn assert_tile_contains_band_values(source: &RasterSource) -> Result<()> {
    let metadata = source.metadata();
    let [west, south, east, north] = metadata.bounds;
    let coordinate = geo::Coordinate::latlon((south + north) / 2.0, (west + east) / 2.0);
    let tile = Tile::for_coordinate(coordinate, metadata.max_zoom);
    let data = source.numeric_tile(tile, 1)?.context("Missing raster tile")?;
    assert!(data.values.iter().any(|value| (source.range().0..=source.range().1).contains(value)));
    Ok(())
}

fn write_netcdf_series(path: &Path, bands: usize) -> Result<()> {
    let mut dataset = gdal::DriverManager::get_driver_by_name("MEM")?.create_with_band_type::<f32, _>("", 16, 16, bands)?;
    dataset.set_geo_transform(&[2_635_554.0, 300.0, 0.0, 1_997_575.0, 0.0, -300.0])?;
    configure_time_dimension(&mut dataset, bands)?;
    for band_nr in 1..=bands {
        write_time_slice(&dataset, band_nr)?;
    }
    save_time_series_fixture(&dataset, path, bands / 2)
}

fn save_time_series_fixture(dataset: &gdal::Dataset, path: &Path, cached: usize) -> Result<()> {
    dataset
        .create_copy(
            &gdal::DriverManager::get_driver_by_name("netCDF")?,
            path,
            &gdal::raster::RasterCreationOptions::from_iter(["FORMAT=NC", "WRITE_BOTTOMUP=NO"]),
        )?
        .close()?;
    std::fs::write(
        path.with_extension("nc.aux.xml"),
        format!(
            "<PAMDataset><PAMRasterBand band=\"{cached}\"><Metadata>\
             <MDI key=\"STATISTICS_MINIMUM\">{cached}</MDI><MDI key=\"STATISTICS_MAXIMUM\">{cached}.5</MDI>\
             <MDI key=\"STATISTICS_MEAN\">{cached}.25</MDI><MDI key=\"STATISTICS_STDDEV\">0.25</MDI>\
             </Metadata></PAMRasterBand></PAMDataset>"
        ),
    )?;
    Ok(())
}

fn configure_time_dimension(dataset: &mut gdal::Dataset, bands: usize) -> Result<()> {
    let values = (0..bands).map(|band| band.to_string()).collect::<Vec<_>>().join(",");
    for (name, value) in [
        ("NETCDF_DIM_EXTRA", "{time}".to_owned()),
        ("NETCDF_DIM_time_DEF", format!("{{{bands},6}}")),
        ("NETCDF_DIM_time_VALUES", format!("{{{values}}}")),
        ("time#units", "hours since 2020-01-01 00:00:00".to_owned()),
    ] {
        dataset.set_metadata_item(name, &value, "")?;
    }
    Ok(())
}

fn write_time_slice(dataset: &gdal::Dataset, band_nr: usize) -> Result<()> {
    let mut band = dataset.rasterband(band_nr)?;
    band.set_metadata_item("NETCDF_VARNAME", "LST", "")?;
    band.set_metadata_item("NETCDF_DIM_time", &(band_nr - 1).to_string(), "")?;
    let values = (0..256).map(|pixel| band_nr as f32 + (pixel % 2) as f32 * 0.5).collect();
    band.write((0, 0), (16, 16), &mut gdal::raster::Buffer::new((16, 16), values))?;
    Ok(())
}
