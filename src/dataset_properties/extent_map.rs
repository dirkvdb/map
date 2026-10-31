use std::num::NonZeroU32;
use std::sync::Arc;

use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::{
    App, Bounds, FontWeight, InteractiveElement, IntoElement, ObjectFit, ParentElement, Pixels, RenderImage, RenderOnce, Role, SharedString,
    StatefulInteractiveElement, Styled, StyledImage, TestSupportExt, Window, canvas, div, fill, img, outline, point, size,
};

use crate::map_camera::{MapCamera, mercator_y};
use crate::map_renderer::{MapRequest, Viewport};

/// Logical size of the static basemap render; the 2:1 frame scales it to fit.
const PREVIEW_WIDTH: NonZeroU32 = NonZeroU32::new(640).unwrap();
const PREVIEW_HEIGHT: NonZeroU32 = NonZeroU32::new(320).unwrap();

mod annotations;

/// Native source grid values, not dimensions or resolution of the preview.
#[derive(Clone, Debug)]
pub(super) struct RasterGrid {
    pub(super) dimensions: SharedString,
    pub(super) corners: [SharedString; 4],
    cell_size: Option<SharedString>,
}

impl RasterGrid {
    /// Corners stay in the source CRS; the cell size line needs known units, which only native COGs supply.
    pub(super) fn source(reference: &geo::GeoReference) -> Self {
        Self {
            dimensions: format!("{}x{}", reference.rows().count(), reference.columns().count()).into(),
            corners: annotations::source_corners(reference),
            cell_size: None,
        }
    }

    pub(super) fn cog(metadata: &geo::geotiff::GeoTiffMetadata) -> Self {
        let reference = &metadata.geo_reference;
        let cell_size = reference.cell_size();
        Self {
            cell_size: Some(format!("Cell size: {:.6} × {:.6} m", cell_size.x().abs(), cell_size.y().abs()).into()),
            ..Self::source(reference)
        }
    }
}

/// The default background rendered once behind the extent; it is never panned.
#[derive(Clone, Default)]
pub(super) enum Basemap {
    #[default]
    Loading,
    Ready(Arc<RenderImage>),
    Unavailable,
}

/// Immutable preview data prepared once when the dialog opens, not on each frame.
/// Extent and viewport share the renderer's normalized Web Mercator coordinates.
#[derive(Clone, IntoElement)]
pub(super) struct ExtentMap {
    extent: Option<Bounds<f64>>,
    camera: Option<MapCamera>,
    viewport: Bounds<f64>,
    basemap: Basemap,
    label: SharedString,
    corners: Option<[SharedString; 4]>,
    grid: Option<RasterGrid>,
}

impl ExtentMap {
    pub(super) fn new(bounds: [f64; 4]) -> Self {
        let extent = normalized_extent(bounds);
        let camera = extent.map(|_| MapCamera::fit_bounds(bounds, f64::from(PREVIEW_WIDTH.get()), f64::from(PREVIEW_HEIGHT.get())));
        let [west, south, east, north] = bounds;
        let label = if extent.is_some() {
            format!("Map showing dataset extent: west {west:.6}°, south {south:.6}°, east {east:.6}°, north {north:.6}°")
        } else {
            "Map. Dataset extent unavailable.".to_owned()
        };
        Self {
            extent,
            camera,
            viewport: camera.map_or(Bounds::new(point(0., 0.), size(1., 1.)), visible_viewport),
            basemap: Basemap::Loading,
            label: label.into(),
            corners: extent.map(|_| annotations::corner_coordinates(bounds)),
            grid: None,
        }
    }

    /// Raster corners replace the WGS84 defaults with the source CRS coordinates.
    pub(super) fn with_grid(mut self, grid: Option<RasterGrid>) -> Self {
        if let Some(grid) = &grid {
            self.label = format!("{}; {} rows × columns", self.label, grid.dimensions).into();
            self.corners = Some(grid.corners.clone());
        }
        if let Some(cell_size) = grid.as_ref().and_then(|grid| grid.cell_size.as_ref()) {
            self.label = format!("{}; {cell_size}; EPSG:3857", self.label).into();
        }
        self.grid = grid;
        self
    }

    pub(super) fn with_basemap(mut self, basemap: Basemap) -> Self {
        self.basemap = basemap;
        self
    }

    /// The single render that supplies the basemap, at the display's pixel density.
    pub(super) fn basemap_request(&self, scale: f32) -> Option<MapRequest> {
        Some(MapRequest {
            viewport: Viewport {
                width: PREVIEW_WIDTH,
                height: PREVIEW_HEIGHT,
                scale,
            },
            camera: self.camera?,
        })
    }

    fn render_map(self, cx: &App) -> impl IntoElement {
        let region = self.extent.map(|extent| relative_region(extent, self.viewport));
        let image = match &self.basemap {
            Basemap::Ready(image) => Some(image.clone()),
            Basemap::Loading | Basemap::Unavailable => None,
        };
        div()
            .id("dataset-extent-map")
            .role(Role::Image)
            .aria_label(self.label)
            .test_support()
            .relative()
            .w_full()
            .aspect_ratio(2.)
            .flex_shrink_0()
            .overflow_hidden()
            .bg(cx.theme().muted)
            .children(image.map(|image| img(image).absolute().size_full().object_fit(ObjectFit::Fill)))
            .child(
                canvas(move |bounds, _, _| region.map(|region| project_region(region, bounds)), paint)
                    .absolute()
                    .size_full(),
            )
            .children(self.grid.as_ref().and_then(|grid| grid.cell_size.as_ref()).map(|_| {
                div()
                    .id("dataset-extent-crs")
                    .aria_label("EPSG:3857")
                    .test_support()
                    .absolute()
                    .top_4()
                    .left_4()
                    .child(Tag::secondary().outline().small().bg(cx.theme().background).child("EPSG:3857"))
            }))
            .children(self.grid.map(|grid| annotations::grid_annotations(grid.dimensions, grid.cell_size, cx)))
    }
}

impl RenderOnce for ExtentMap {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let available = self.extent.is_some();
        let basemap_unavailable = available && matches!(self.basemap, Basemap::Unavailable);
        let corners = self.corners.clone();

        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_2()
            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Geospatial extent"))
            .children(corners.as_ref().map(|corners| annotations::corner_row(true, corners, cx)))
            .child(self.render_map(cx))
            .children(corners.as_ref().map(|corners| annotations::corner_row(false, corners, cx)))
            .children((!available).then(|| note("dataset-extent-unavailable", "Extent unavailable", cx)))
            .children(basemap_unavailable.then(|| note("dataset-extent-basemap-unavailable", "Basemap unavailable", cx)))
    }
}

fn note(id: &'static str, text: &'static str, cx: &App) -> impl IntoElement {
    div().id(id).test_support().text_xs().text_color(cx.theme().muted_foreground).child(text)
}

/// Normalized Web Mercator coordinates shared with the rendered basemap.
/// Crossing extents remain contiguous; latitudes beyond the Mercator cap sit on the edge.
fn normalized_extent([west, south, east, north]: [f64; 4]) -> Option<Bounds<f64>> {
    let span = east - west;
    if ![west, south, east, north, span].iter().all(|value| value.is_finite()) || south > north || south > 90.0 || north < -90.0 {
        return None;
    }
    let top = mercator_y(north);
    let height = mercator_y(south) - top;
    if span.abs() >= 360.0 {
        return Some(Bounds::new(point(0., top), size(1., height)));
    }
    let left = (west + 180.0).rem_euclid(360.0) / 360.0;
    let width = span.rem_euclid(360.0) / 360.0;
    Some(Bounds::new(point(left, top), size(width, height)))
}

fn visible_viewport(camera: MapCamera) -> Bounds<f64> {
    let [left, top, right, bottom] = camera.visible_region(f64::from(PREVIEW_WIDTH.get()), f64::from(PREVIEW_HEIGHT.get()));
    Bounds::new(point(left, top), size(right - left, bottom - top))
}

/// The extent moves by whole worlds to the copy nearest the camera, as the basemap repeats.
fn relative_region(extent: Bounds<f64>, viewport: Bounds<f64>) -> Bounds<f32> {
    let shift = (viewport.left() + viewport.size.width / 2. - extent.left() - extent.size.width / 2.).round();
    Bounds::new(
        point(
            ((extent.left() + shift - viewport.left()) / viewport.size.width) as f32,
            ((extent.top() - viewport.top()) / viewport.size.height) as f32,
        ),
        size(
            (extent.size.width / viewport.size.width) as f32,
            (extent.size.height / viewport.size.height) as f32,
        ),
    )
}

fn project_region(region: Bounds<f32>, map: Bounds<Pixels>) -> Bounds<Pixels> {
    Bounds::new(
        map.origin + point(map.size.width * region.origin.x, map.size.height * region.origin.y),
        size(map.size.width * region.size.width, map.size.height * region.size.height),
    )
}

fn paint(bounds: Bounds<Pixels>, region: Option<Bounds<Pixels>>, window: &mut Window, cx: &mut App) {
    // The frame is a physical hairline; extent strokes and markers scale with
    // rem. All geometry is resolved afresh after resize or UI zoom.
    if let Some(region) = region {
        paint_extent(region, window, cx);
    }
    window.paint_quad(outline(bounds, cx.theme().border, Default::default()));
}

fn paint_extent(region: Bounds<Pixels>, window: &mut Window, cx: &App) {
    let stroke = window.rem_size() * 0.125;
    let marker_size = window.rem_size() * 0.5;
    if region.size.width < marker_size && region.size.height < marker_size {
        let marker = centered_minimum(region, marker_size);
        window.paint_quad(
            fill(marker, cx.theme().primary)
                .corner_radii(marker_size / 2.)
                .border_widths(stroke)
                .border_color(cx.theme().background),
        );
        return;
    }
    let region = centered_minimum(region, stroke);
    window.paint_quad(
        fill(region, cx.theme().primary.opacity(0.20))
            .border_widths(stroke)
            .border_color(cx.theme().primary),
    );
}

fn centered_minimum(bounds: Bounds<Pixels>, minimum: Pixels) -> Bounds<Pixels> {
    let size = size(bounds.size.width.max(minimum), bounds.size.height.max(minimum));
    Bounds::new(bounds.center() - point(size.width / 2., size.height / 2.), size)
}

#[cfg(test)]
mod tests {
    use gpui_kit::px;

    use super::*;

    const FRAME: Bounds<Pixels> = Bounds {
        origin: point(px(0.), px(0.)),
        size: size(px(640.), px(320.)),
    };

    fn wrapped(longitude: f64) -> f64 {
        (longitude + 180.).rem_euclid(360.) - 180.
    }

    #[test]
    fn australia_extent_is_centered_and_fills_eighty_five_percent_of_the_frame_height() {
        let preview = ExtentMap::new([112., -44., 154., -10.]);
        let map = Bounds::new(point(px(10.), px(20.)), size(px(720.), px(360.)));
        let extent = project_region(relative_region(preview.extent.unwrap(), preview.viewport), map);
        assert!((extent.center().x - map.center().x).abs() < px(0.001));
        assert!((extent.center().y - map.center().y).abs() < px(0.001));
        assert!((extent.size.height - map.size.height * 0.85).abs() < px(0.01));
        assert!(extent.size.width < map.size.width * 0.85);
    }

    #[test]
    fn extent_overlay_corners_match_the_coordinates_the_basemap_camera_renders_there() {
        for bounds in [
            [112., -44., 154., -10.],
            [-140., 30., -60., 40.],
            [10., -50., 20., 60.],
            [170., -10., -170., 10.],
            [-190., -10., -170., 10.],
            [4., 51., 4.0001, 51.0001],
            [-10., 80., 10., 85.],
        ] {
            let preview = ExtentMap::new(bounds);
            let camera = preview.camera.unwrap();
            let region = project_region(relative_region(preview.extent.unwrap(), preview.viewport), FRAME);
            let offset = |corner: gpui_kit::Point<Pixels>| (f64::from(corner.x - FRAME.center().x), f64::from(corner.y - FRAME.center().y));
            let north_west = camera.coordinate_at(offset(region.origin));
            let south_east = camera.coordinate_at(offset(region.bottom_right()));
            let [west, south, east, north] = bounds;
            // Logical pixels are f32; a ten-thousandth of a degree is far below one pixel here.
            assert!((north_west.longitude - wrapped(west)).abs() < 1e-4, "West edge misplaced for {bounds:?}");
            assert!((north_west.latitude - north).abs() < 1e-4, "North edge misplaced for {bounds:?}");
            assert!((south_east.longitude - wrapped(east)).abs() < 1e-4, "East edge misplaced for {bounds:?}");
            assert!((south_east.latitude - south).abs() < 1e-4, "South edge misplaced for {bounds:?}");
        }
    }

    #[test]
    fn fitting_keeps_wide_tall_tiny_and_dateline_extents_centered_within_the_frame() {
        for bounds in [
            [-140., 30., -60., 40.],
            [10., -50., 20., 60.],
            [4., 51., 4.0001, 51.0001],
            [170., -10., -170., 10.],
        ] {
            let preview = ExtentMap::new(bounds);
            let region = relative_region(preview.extent.unwrap(), preview.viewport);
            assert!(region.size.width.max(region.size.height) <= 0.85 + 1e-6, "Incorrect fit for {bounds:?}");
            assert!((region.center().x - 0.5).abs() < 1e-6, "Off-centre fit for {bounds:?}");
            assert!((region.center().y - 0.5).abs() < 1e-6, "Off-centre fit for {bounds:?}");
            assert!(region.left() >= 0.075 - 1e-6 && region.right() <= 0.925 + 1e-6);
            assert!(region.top() >= 0.075 - 1e-6 && region.bottom() <= 0.925 + 1e-6);
        }
    }

    #[test]
    fn global_extents_span_one_world_at_the_minimum_zoom_and_overflow_the_mercator_caps() {
        let preview = ExtentMap::new([-180., -90., 180., 90.]);
        let region = relative_region(preview.extent.unwrap(), preview.viewport);
        assert!((region.size.width - 512. / 640.).abs() < 1e-6, "One world is 512 logical pixels at zoom zero");
        assert!((region.center().x - 0.5).abs() < 1e-6 && (region.center().y - 0.5).abs() < 1e-6);
        assert!(region.top() < 0. && region.bottom() > 1., "The 2:1 frame cannot show the full Mercator height");
    }

    #[test]
    fn antimeridian_extents_are_fitted_as_one_contiguous_region() {
        for bounds in [[170., -10., -170., 10.], [170., -10., 190., 10.], [-190., -10., -170., 10.]] {
            let extent = normalized_extent(bounds).unwrap();
            assert!(extent.left() < 1. && extent.right() > 1.);
            assert!((extent.size.width - 20. / 360.).abs() < 1e-6);
            let region = relative_region(extent, ExtentMap::new(bounds).viewport);
            assert!((region.center().x - 0.5).abs() < 1e-6, "Dateline extent misplaced for {bounds:?}");
            assert!((region.size.height - 0.85).abs() < 1e-6);
        }
    }

    #[test]
    fn basemap_requests_use_the_fixed_preview_viewport_and_the_fitted_camera() {
        let request = ExtentMap::new([112., -44., 154., -10.]).basemap_request(2.).unwrap();
        assert_eq!(
            (request.viewport.width.get(), request.viewport.height.get(), request.viewport.scale),
            (640, 320, 2.)
        );
        assert_eq!(request.camera, MapCamera::fit_bounds([112., -44., 154., -10.], 640., 320.));
        assert!(ExtentMap::new([f64::NAN, 0., 1., 1.]).basemap_request(1.).is_none());
    }

    #[test]
    fn native_grid_annotations_show_rows_before_columns_and_positive_cell_sizes() {
        let (_directory, sources) = crate::raster_source::test_support::cog_fixture(1).unwrap();
        let mut metadata = sources[0].cog_reader().unwrap().cog_metadata().clone();
        metadata.geo_reference.set_rows(geo::Rows(128));
        metadata.geo_reference.set_columns(geo::Columns(512));
        let grid = RasterGrid::cog(&metadata);
        assert_eq!(grid.dimensions.as_ref(), "128x512");
        assert_eq!(grid.cell_size.as_deref(), Some("Cell size: 19567.879241 × 19567.879241 m"));
        let preview = ExtentMap::new([112., -44., 154., -10.]).with_grid(Some(grid.clone()));
        assert!(preview.label.contains("128x512 rows × columns"));
        assert!(preview.label.contains("Cell size:"));
        assert!(preview.label.contains("EPSG:3857"));
        assert_eq!(preview.corners, Some(grid.corners), "Raster previews must show source CRS corners, not WGS84");
    }

    #[test]
    fn source_grid_annotations_show_dimensions_and_projected_corners_without_inventing_a_cell_size() {
        let reference = geo::GeoReference::with_top_left_origin(
            geo::srs::projection_from_epsg(geo::crs::Epsg::new(3035)).unwrap(),
            geo::RasterSize::with_rows_cols(geo::Rows(69), geo::Columns(129)),
            geo::Point::new(2_500_000.5, 2_000_000.25),
            geo::CellSize::square(1000.),
            None::<f64>,
        );
        let grid = RasterGrid::source(&reference);
        assert_eq!(grid.dimensions.as_ref(), "69x129");
        assert!(grid.cell_size.is_none());
        assert_eq!(grid.corners[0].as_ref(), "2500000.50, 2000000.25");
        assert_eq!(grid.corners[1].as_ref(), "2629000.50, 2000000.25");
        assert_eq!(grid.corners[2].as_ref(), "2500000.50, 1931000.25");
        assert_eq!(grid.corners[3].as_ref(), "2629000.50, 1931000.25");
        let preview = ExtentMap::new([-9.6, 38.5, -8.4, 39.1]).with_grid(Some(grid.clone()));
        assert!(preview.label.contains("69x129 rows × columns"));
        assert!(!preview.label.contains("Cell size:"));
        assert_eq!(preview.corners, Some(grid.corners));
    }

    #[test]
    fn geographic_source_grids_keep_degree_corners() {
        let reference = geo::GeoReference::with_top_left_origin(
            geo::srs::projection_from_epsg(geo::crs::epsg::WGS84).unwrap(),
            geo::RasterSize::with_rows_cols(geo::Rows(10), geo::Columns(20)),
            geo::Point::new(4., 52.),
            geo::CellSize::square(0.1),
            None::<f64>,
        );
        let corners = RasterGrid::source(&reference).corners;
        assert_eq!(corners[0].as_ref(), "4.0000°E, 52.0000°N");
        assert_eq!(corners[3].as_ref(), "6.0000°E, 51.0000°N");
    }

    #[test]
    fn point_and_line_extents_remain_visible_without_moving_their_centres() {
        for bounds in [[4., 51., 4., 51.], [-10., 0., 10., 0.]] {
            let preview = ExtentMap::new(bounds);
            let relative = relative_region(preview.extent.unwrap(), preview.viewport);
            let region = project_region(relative, FRAME);
            let visible = centered_minimum(region, px(8.));
            assert_eq!(region.center(), visible.center());
            assert!(visible.size.width >= px(8.) && visible.size.height >= px(8.));
        }
    }

    #[test]
    fn invalid_extents_are_not_silently_displayed_at_null_island() {
        for bounds in [
            [f64::NAN, 0., 1., 1.],
            [0., 0., f64::INFINITY, 1.],
            [0., 10., 1., -10.],
            [0., 91., 1., 92.],
            [0., -92., 1., -91.],
        ] {
            assert!(normalized_extent(bounds).is_none());
            assert!(ExtentMap::new(bounds).camera.is_none());
        }
    }
}
