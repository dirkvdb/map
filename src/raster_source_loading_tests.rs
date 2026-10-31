use std::collections::HashSet;

use super::*;

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
