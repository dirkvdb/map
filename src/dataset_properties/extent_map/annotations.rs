use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{App, InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement, Styled, TestSupportExt, div};

/// Corners in the source CRS: projected grids show eastings and northings, geographic grids degrees.
pub(super) fn source_corners(reference: &geo::GeoReference) -> [SharedString; 4] {
    let projected = geo::srs::SpatialReference::from_definition(reference.projection()).is_ok_and(|srs| srs.is_projected());
    if !projected {
        let (north_west, south_east) = (reference.top_left(), reference.bottom_right());
        return corner_coordinates([north_west.x(), south_east.y(), south_east.x(), north_west.y()]);
    }
    [reference.top_left(), reference.top_right(), reference.bottom_left(), reference.bottom_right()]
        .map(|corner| format!("{:.2}, {:.2}", corner.x(), corner.y()).into())
}

pub(super) fn corner_coordinates([west, south, east, north]: [f64; 4]) -> [SharedString; 4] {
    [(west, north), (east, north), (west, south), (east, south)].map(|(longitude, latitude)| {
        let wrapped = (longitude + 180.).rem_euclid(360.) - 180.;
        let longitude = if wrapped == -180. && longitude > 0. { 180. } else { wrapped };
        format!(
            "{:.4}°{}, {:.4}°{}",
            longitude.abs(),
            if longitude < 0. { "W" } else { "E" },
            latitude.abs(),
            if latitude < 0. { "S" } else { "N" },
        )
        .into()
    })
}

pub(super) fn corner_row(north: bool, corners: &[SharedString; 4], cx: &App) -> impl IntoElement {
    let corners = if north {
        [("dataset-extent-nw", "NW", &corners[0]), ("dataset-extent-ne", "NE", &corners[1])]
    } else {
        [("dataset-extent-sw", "SW", &corners[2]), ("dataset-extent-se", "SE", &corners[3])]
    };
    div()
        .flex()
        .items_start()
        .gap_2()
        .children(corners.into_iter().enumerate().map(|(side, (id, name, coordinates))| {
            let label = SharedString::from(format!("{name} {coordinates}"));
            div()
                .id(id)
                .aria_label(label.clone())
                .test_support()
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .when(side == 1, |corner| corner.text_right())
                .child(label)
        }))
}

pub(super) fn grid_annotations(dimensions: SharedString, cell_size_text: Option<SharedString>, cx: &App) -> impl IntoElement {
    div()
        .absolute()
        .left_4()
        .right_4()
        .bottom_4()
        .flex()
        .flex_col()
        .items_start()
        .gap_2()
        .child(
            div()
                .id("dataset-extent-dimensions")
                .aria_label(dimensions.clone())
                .test_support()
                .min_w_0()
                .max_w_full()
                .child(Tag::secondary().outline().small().bg(cx.theme().background).child(dimensions)),
        )
        .children(cell_size_text.map(|text| cell_size(text, cx)))
}

fn cell_size(text: SharedString, cx: &App) -> impl IntoElement {
    div()
        .id("dataset-extent-cell-size")
        .aria_label(text.clone())
        .test_support()
        .max_w_full()
        .child(Tag::secondary().outline().small().bg(cx.theme().background).child(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corner_coordinates_preserve_north_south_and_east_west_order() {
        assert_eq!(
            corner_coordinates([112., -44., 154., -10.]),
            [
                SharedString::from("112.0000°E, 10.0000°S"),
                SharedString::from("154.0000°E, 10.0000°S"),
                SharedString::from("112.0000°E, 44.0000°S"),
                SharedString::from("154.0000°E, 44.0000°S"),
            ]
        );
    }

    #[test]
    fn corner_coordinates_wrap_longitudes_without_changing_the_extent_sides() {
        let corners = corner_coordinates([170., -10., 190., 10.]);
        assert_eq!(corners[0].as_ref(), "170.0000°E, 10.0000°N");
        assert_eq!(corners[1].as_ref(), "170.0000°W, 10.0000°N");
        let world = corner_coordinates([-180., -90., 180., 90.]);
        assert_eq!(world[0].as_ref(), "180.0000°W, 90.0000°N");
        assert_eq!(world[1].as_ref(), "180.0000°E, 90.0000°N");
    }
}
