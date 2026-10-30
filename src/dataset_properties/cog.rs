use geo::cog::WebTilesReader;
use geo::geotiff::GeoTiffMetadata;
use geo::raster::{Compression, Predictor};

use super::{PropertyRows, bounds_rows, pair};

pub(super) fn sections(reader: &WebTilesReader, max_zoom: i32) -> Vec<(&'static str, PropertyRows)> {
    let metadata = reader.cog_metadata();
    let mut sections = vec![
        ("COG storage", storage_rows(metadata)),
        ("Image pyramid", overview_rows(reader, max_zoom)),
        ("Tile coverage", bounds_rows(reader.data_bounds().array())),
    ];
    if let Some(bounds) = metadata.data_bounds {
        sections.push(("Valid data extent", bounds_rows(bounds)));
    }
    // The reader's statistics are not keyed by band, so they cannot reliably
    // describe the selected band of a multiband file.
    if metadata.band_count == 1
        && let Some(statistics) = &metadata.statistics
    {
        sections.push((
            "Stored statistics",
            vec![
                pair("Minimum", statistics.minimum_value),
                pair("Maximum", statistics.maximum_value),
                pair("Mean", statistics.mean),
                pair("Standard deviation", statistics.standard_deviation),
                pair("Valid pixels", format!("{}%", statistics.valid_pixel_percentage)),
            ],
        ));
    }
    sections
}

fn storage_rows(metadata: &GeoTiffMetadata) -> PropertyRows {
    let reference = &metadata.geo_reference;
    let pixel_size = reference.cell_size();
    vec![
        pair("Dimensions", format!("{} × {} px", reference.columns().count(), reference.rows().count())),
        pair("Pixel size", format!("{:.6} × {:.6} m", pixel_size.x().abs(), pixel_size.y().abs())),
        pair("Compression", compression_name(metadata.compression)),
        pair(
            "Predictor",
            match metadata.predictor {
                None => "None",
                Some(Predictor::Horizontal) => "Horizontal differencing",
                Some(Predictor::FloatingPoint) => "Floating point",
            },
        ),
        pair("Interleave", format!("{:?}", metadata.interleave)),
        pair("Overviews", metadata.overviews.len().saturating_sub(1)),
    ]
}

fn compression_name(compression: Option<Compression>) -> &'static str {
    match compression {
        None => "None",
        Some(Compression::Lzw) => "LZW",
        Some(Compression::Zstd) => "Zstandard",
        Some(Compression::Deflate) => "DEFLATE",
        Some(Compression::Lerc) => "LERC",
        Some(Compression::LercDeflate) => "LERC + DEFLATE",
        Some(Compression::LercZstd) => "LERC + Zstandard",
    }
}

fn overview_rows(reader: &WebTilesReader, max_zoom: i32) -> PropertyRows {
    reader
        .native_overviews()
        .iter()
        .enumerate()
        .map(|(index, overview)| {
            let zoom = max_zoom - index as i32;
            let label = if index == 0 { format!("Base · zoom {zoom}") } else { format!("Zoom {zoom}") };
            pair(
                label,
                format!("{} × {} px", overview.raster_size.cols.count(), overview.raster_size.rows.count()),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use geo::geotiff::TiffStats;

    use super::*;

    fn metadata(bands: usize) -> GeoTiffMetadata {
        let (_directory, sources) = crate::raster_source::test_support::cog_fixture(bands).unwrap();
        sources[0].cog_reader().unwrap().cog_metadata().clone()
    }

    #[test]
    fn cog_properties_include_storage_and_each_native_image_level() {
        let mut metadata = metadata(2);
        let mut overview = metadata.overviews[0].clone();
        overview.raster_size.cols.0 /= 2;
        overview.raster_size.rows.0 /= 2;
        overview.chunk_locations.truncate(2);
        metadata.overviews.push(overview);
        metadata.predictor = Some(Predictor::FloatingPoint);
        let reader = WebTilesReader::new(metadata).unwrap();
        let sections = sections(&reader, 3);
        let storage = &sections.iter().find(|(title, _)| *title == "COG storage").unwrap().1;
        for expected in [
            pair("Dimensions", "512 × 512 px"),
            pair("Pixel size", "19567.879241 × 19567.879241 m"),
            pair("Compression", "DEFLATE"),
            pair("Predictor", "Floating point"),
            pair("Interleave", "Band"),
            pair("Overviews", 1),
        ] {
            assert!(storage.contains(&expected), "Missing property: {expected:?}; storage: {storage:?}");
        }
        assert_eq!(
            sections.iter().find(|(title, _)| *title == "Image pyramid").unwrap().1,
            vec![pair("Base · zoom 3", "512 × 512 px"), pair("Zoom 2", "256 × 256 px")]
        );
    }

    #[test]
    fn cog_properties_distinguish_tile_coverage_from_optional_valid_data_bounds() {
        let mut metadata = metadata(1);
        let bounds = [-1.0, -2.0, 3.0, 4.0];
        metadata.data_bounds = Some(bounds);
        let reader = WebTilesReader::new(metadata).unwrap();
        let sections = sections(&reader, 3);
        let coverage = &sections.iter().find(|(title, _)| *title == "Tile coverage").unwrap().1;
        let valid = &sections.iter().find(|(title, _)| *title == "Valid data extent").unwrap().1;
        assert_eq!(*coverage, bounds_rows(reader.data_bounds().array()));
        assert_eq!(*valid, bounds_rows(bounds));
        assert_ne!(coverage, valid);
    }

    #[test]
    fn cog_properties_omit_missing_bounds_and_statistics() {
        let mut metadata = metadata(1);
        metadata.data_bounds = None;
        metadata.statistics = None;
        metadata.compression = None;
        metadata.predictor = None;
        let reader = WebTilesReader::new(metadata).unwrap();
        let sections = sections(&reader, 3);
        assert!(!sections.iter().any(|(title, _)| matches!(*title, "Valid data extent" | "Stored statistics")));
        let storage = &sections.iter().find(|(title, _)| *title == "COG storage").unwrap().1;
        assert!(storage.contains(&pair("Compression", "None")));
        assert!(storage.contains(&pair("Predictor", "None")));
        assert!(storage.contains(&pair("Overviews", 0)));
    }

    #[test]
    fn cog_statistics_remain_raw_and_are_not_attributed_to_arbitrary_bands() {
        for bands in [1, 2] {
            let mut metadata = metadata(bands);
            metadata.statistics = Some(TiffStats {
                minimum_value: -10.0,
                maximum_value: 42.0,
                mean: 12.5,
                standard_deviation: 3.25,
                valid_pixel_percentage: 87.5,
            });
            let reader = WebTilesReader::new(metadata).unwrap();
            let sections = sections(&reader, 3);
            let statistics = sections.iter().find(|(title, _)| *title == "Stored statistics");
            if bands > 1 {
                assert!(statistics.is_none(), "The reader does not retain per-band statistics");
                continue;
            }
            assert_eq!(
                statistics.unwrap().1,
                vec![
                    pair("Minimum", -10),
                    pair("Maximum", 42),
                    pair("Mean", 12.5),
                    pair("Standard deviation", 3.25),
                    pair("Valid pixels", "87.5%"),
                ]
            );
        }
    }
}
