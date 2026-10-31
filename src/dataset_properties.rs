use std::path::Path;

use gpui_kit::component::description_list::{DescriptionItem, DescriptionList};
use gpui_kit::component::{ActiveTheme, Sizable, WindowExt};
use gpui_kit::{App, AppContext, FontWeight, InteractiveElement, IntoElement, ParentElement, SharedString, Styled, TestSupportExt, Window, div, rems};

use crate::map_renderer::MapRenderer;
use crate::raster_source::RasterSource;
use crate::vector_source::VectorSource;

mod cog;
mod extent_map;
mod extent_preview;

gpui_kit::actions!(dataset_properties, [OpenDatasetProperties]);

type PropertyRows = Vec<(SharedString, SharedString)>;

/// A cheap, immutable snapshot of the selected layer's retained metadata.
#[derive(Default)]
pub(crate) struct DatasetProperties {
    sections: Vec<(&'static str, PropertyRows)>,
    extent: Option<[f64; 4]>,
    preview_grid: Option<extent_map::RasterGrid>,
}

impl DatasetProperties {
    pub(crate) fn raster(source: &RasterSource, path: &Path, band_count: usize) -> Self {
        let metadata = source.metadata();
        let mut general = vec![
            pair("Name", &metadata.name),
            pair("Source", path.display()),
            pair("Format", raster_format(metadata.source_format)),
            pair("Bands", band_count),
        ];
        if !metadata.description.is_empty() {
            general.push(pair("Description", &metadata.description));
        }
        let mut spatial = bounds_rows(metadata.bounds);
        if let Some(epsg) = metadata.epsg {
            spatial.insert(0, pair("Source CRS", epsg));
        }
        let mut sections = vec![("Dataset", general), ("Spatial reference", spatial)];
        sections.push(("Raster", raster_rows(source)));
        if let Some(reader) = source.cog_reader() {
            sections.extend(cog::sections(reader, metadata.max_zoom));
        }
        let mut additional = metadata.additional_data.iter().map(|(key, value)| pair(key, value)).collect::<Vec<_>>();
        additional.sort_by(|left, right| left.0.cmp(&right.0));
        if !additional.is_empty() {
            sections.push(("Metadata", additional));
        }
        Self {
            sections,
            extent: Some(metadata.bounds),
            preview_grid: preview_grid(source),
        }
    }

    pub(crate) fn vector(source: &VectorSource) -> Self {
        let mut general = vec![
            pair("Name", source.name()),
            pair("Type", "Vector"),
            pair("Rendering", if source.tile_url().is_some() { "Streamed tiles" } else { "WGS84 GeoJSON" }),
        ];
        if let Some(url) = source.tile_url() {
            general.push(pair("Source", url));
        }
        let mut sections = vec![("Dataset", general), ("Spatial reference", bounds_rows(source.bounds()))];
        let attributes = source
            .numeric_properties()
            .iter()
            .map(|property| pair("Numeric attribute", property.name()))
            .collect::<Vec<_>>();
        if !attributes.is_empty() {
            sections.push(("Attributes", attributes));
        }
        Self {
            sections,
            extent: Some(source.bounds()),
            preview_grid: None,
        }
    }

    /// The preview's basemap renders on `basemap_renderer`; without one the
    /// extent is drawn over a plain frame.
    pub(crate) fn open(self, basemap_renderer: Option<MapRenderer>, window: &mut Window, cx: &mut App) {
        let preview = self.extent.map(|extent| {
            let map = extent_map::ExtentMap::new(extent).with_grid(self.preview_grid.clone());
            cx.new(|cx| extent_preview::ExtentPreview::new(map, basemap_renderer, window, cx))
        });
        window.open_dialog(cx, move |dialog, window, cx| {
            dialog.title("Dataset properties").w(window.rem_size() * 50.).max_w_full().child(
                div()
                    .id("dataset-properties")
                    .test_support()
                    .flex()
                    .flex_col()
                    .flex_shrink_0()
                    .text_sm()
                    .gap_6()
                    .children(preview.clone())
                    .children(self.sections.iter().map(|(heading, rows)| section(heading, rows, cx)))
                    .children(self.sections.is_empty().then(|| {
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("No dataset selected. Open a dataset to inspect its properties.")
                    })),
            )
        });
    }
}

fn pair(label: impl std::fmt::Display, value: impl std::fmt::Display) -> (SharedString, SharedString) {
    (label.to_string().into(), value.to_string().into())
}

/// Native COGs also report their cell size; other rasters only retain their grid dimensions.
fn preview_grid(source: &RasterSource) -> Option<extent_map::RasterGrid> {
    source
        .cog_reader()
        .map(|reader| extent_map::RasterGrid::cog(reader.cog_metadata()))
        .or_else(|| source.metadata().geo_reference.as_deref().map(extent_map::RasterGrid::source))
}

fn bounds_rows([west, south, east, north]: [f64; 4]) -> PropertyRows {
    vec![
        pair("Bounds CRS", "EPSG:4326 · WGS84"),
        pair("West / east", format!("{west:.6} / {east:.6}")),
        pair("South / north", format!("{south:.6} / {north:.6}")),
    ]
}

fn raster_rows(source: &RasterSource) -> PropertyRows {
    let metadata = source.metadata();
    let mut rows = vec![pair("Zoom levels", format!("{}–{}", metadata.min_zoom, metadata.max_zoom))];
    if metadata.band_nr.is_some() {
        rows.extend([
            pair("Data type", metadata.data_type),
            pair("Stored range", format!("{} – {}", metadata.min_value, metadata.max_value)),
        ]);
    }
    if let Some(nodata) = metadata.nodata {
        rows.push(pair("No data", nodata));
    }
    if let Some(scale) = metadata.scale {
        rows.extend([pair("Scale", scale.scale), pair("Offset", scale.offset)]);
    }
    if let Some(size) = metadata.tile_size {
        rows.push(pair("Tile size", format!("{size} × {size}")));
    }
    rows
}

fn raster_format(format: tiler::LayerSourceType) -> &'static str {
    use tiler::LayerSourceType;
    match format {
        LayerSourceType::GeoPackage => "GeoPackage",
        LayerSourceType::Mbtiles => "MBTiles",
        LayerSourceType::ArcAscii => "Arc/Info ASCII grid",
        LayerSourceType::GeoTiff => "GDAL raster",
        LayerSourceType::CloudOptimizedGeoTiff => "Cloud-optimized GeoTIFF",
        LayerSourceType::Netcdf => "NetCDF",
        LayerSourceType::Unknown => "Unknown",
    }
}

fn section(heading: &'static str, rows: &[(SharedString, SharedString)], cx: &App) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .gap_2()
        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(heading))
        .child(
            DescriptionList::horizontal()
                .columns(1)
                .label_width(rems(8.))
                .small()
                .bordered(false)
                .children(rows.iter().map(|(label, value)| {
                    DescriptionItem::new(label.clone()).value(div().min_w_0().text_color(cx.theme().foreground).child(value.clone()).into_any_element())
                })),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raster_properties_show_original_dataset_and_stored_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("elevation.tif");
        crate::raster_source::test_support::write_raster(&path, 3857, 2).unwrap();
        let sources = RasterSource::open(&path).unwrap();
        let properties = DatasetProperties::raster(&sources[1], Path::new("original.nc"), sources.len());
        assert_eq!(properties.extent, Some(sources[1].metadata().bounds));
        let grid = properties.preview_grid.as_ref().expect("GDAL rasters retain their grid dimensions");
        let reference = sources[1].metadata().geo_reference.as_ref().unwrap();
        assert_eq!(
            grid.dimensions.as_ref(),
            format!("{}x{}", reference.rows().count(), reference.columns().count())
        );
        assert_eq!(
            grid.corners[0].as_ref(),
            format!("{:.2}, {:.2}", reference.top_left().x(), reference.top_left().y()),
            "Corners must use the EPSG:3857 source units"
        );
        let rows = properties.sections.iter().flat_map(|(_, rows)| rows).collect::<Vec<_>>();
        for expected in [
            pair("Source", "original.nc"),
            pair("Bands", 2),
            pair("Data type", sources[1].metadata().data_type),
            pair("Source CRS", "EPSG:3857"),
        ] {
            assert!(rows.contains(&&expected), "Missing property: {expected:?}");
        }
        assert!(rows.iter().all(|(label, _)| label.as_ref() != "Selected band"));
        assert!(!properties.sections.iter().any(|(title, _)| *title == "COG storage"));
    }

    #[test]
    fn cog_properties_use_retained_metadata_after_the_file_is_moved() {
        let (_directory, sources) = crate::raster_source::test_support::cog_fixture(2).unwrap();
        let source = &sources[1];
        let path = source.metadata().path.clone();
        std::fs::rename(&path, path.with_extension("moved.tif")).unwrap();
        let restyled = source.restyled(inf::colormap::ColorMapPreset::Gray, (0.0, 255.0)).unwrap();
        let properties = DatasetProperties::raster(&restyled, &path, sources.len());
        assert!(properties.preview_grid.is_some(), "Preview grid metadata must not require reopening the source");
        assert!(properties.sections.iter().any(|(title, _)| *title == "COG storage"));
        assert!(properties.sections.iter().any(|(title, _)| *title == "Image pyramid"));
        assert!(
            properties
                .sections
                .iter()
                .flat_map(|(_, rows)| rows)
                .all(|(label, _)| label.as_ref() != "Selected band")
        );
    }

    #[test]
    fn vector_properties_include_numeric_attributes_without_parsing_geojson_again() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("points.geojson");
        std::fs::write(
            &path,
            r#"{"type":"FeatureCollection","features":[{"type":"Feature","geometry":{"type":"Point","coordinates":[4,51]},"properties":{"height":12}}]}"#,
        )
        .unwrap();
        let sources = VectorSource::open(&path).unwrap().unwrap();
        let properties = DatasetProperties::vector(&sources[0]);
        assert_eq!(properties.extent, Some([4., 51., 4., 51.]));
        assert!(
            properties.preview_grid.is_none(),
            "Vector previews must not invent raster dimensions or cell sizes"
        );
        assert!(
            properties
                .sections
                .iter()
                .any(|(title, rows)| *title == "Attributes" && rows.contains(&pair("Numeric attribute", "height")))
        );
    }
}
