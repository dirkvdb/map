use std::f64::consts::PI;

use maplibre_native::{CameraUpdate, LatLng};

const MIN_ZOOM: f64 = 0.0;
const MAX_ZOOM: f64 = 18.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MapCamera {
    // Normalized Web Mercator coordinates; one world spans 512 logical pixels
    // at zoom zero, independent of display DPI.
    x: f64,
    y: f64,
    zoom: f64,
}

impl Default for MapCamera {
    fn default() -> Self {
        Self {
            x: 0.5,
            y: (1.0 - 20.0_f64.to_radians().tan().asinh() / PI) / 2.0,
            zoom: 1.5,
        }
    }
}

impl MapCamera {
    /// Convert a logical-pixel offset from the viewport center to WGS84.
    pub fn coordinate_at(self, offset: (f64, f64)) -> geo::Coordinate {
        let x = (self.x + offset.0 / self.world_size()).rem_euclid(1.0);
        let y = (self.y + offset.1 / self.world_size()).clamp(0.0, 1.0);
        geo::Coordinate::latlon((PI * (1.0 - 2.0 * y)).sinh().atan().to_degrees(), x * 360.0 - 180.0)
    }

    /// Test whether WGS84 `[west, south, east, north]` bounds overlap the north-up,
    /// unpitched viewport, accounting for repeated longitude worlds.
    /// Uses `geo::LatLonBounds::intersects`: touching-only edges and bounds entirely
    /// beyond the Mercator latitude cap do not intersect; interior point bounds do.
    /// Viewport dimensions are logical pixels. Nonfinite inputs, nonpositive
    /// dimensions, and inverted latitude bounds do not intersect.
    pub fn intersects_bounds(self, bounds: [f64; 4], width: f64, height: f64) -> bool {
        let [west, south, east, north] = bounds;
        if !bounds.into_iter().all(f64::is_finite) || !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 || south > north {
            return false;
        }

        let half_width = width / self.world_size() / 2.0;
        // Keep longitude edges unwrapped: the viewport can span multiple worlds.
        let viewport = geo::LatLonBounds::from([
            (self.x - half_width) * 360.0 - 180.0,
            self.coordinate_at((0.0, height / 2.0)).latitude,
            (self.x + half_width) * 360.0 - 180.0,
            self.coordinate_at((0.0, -height / 2.0)).latitude,
        ]);
        let east = if east < west { east + 360.0 } else { east };
        let center_longitude = self.x * 360.0 - 180.0;
        let shift = ((center_longitude - (west + east) / 2.0) / 360.0).round() * 360.0;
        let dataset = geo::LatLonBounds::from([west + shift, south, east + shift, north]);
        viewport.intersects(&dataset)
    }

    pub fn fit_bounds(bounds: [f64; 4], width: f64, height: f64) -> Self {
        let [west, south, east, north] = bounds;
        let project_y = |latitude: f64| (1.0 - latitude.clamp(-85.05112878, 85.05112878).to_radians().tan().asinh() / PI) / 2.0;
        let left = (west + 180.0) / 360.0;
        let mut right = (east + 180.0) / 360.0;
        if right < left {
            right += 1.0;
        }
        let top = project_y(north);
        let bottom = project_y(south);
        let zoom_x = (width.max(1.0) * 0.85 / (512.0 * (right - left).max(1e-12))).log2();
        let zoom_y = (height.max(1.0) * 0.85 / (512.0 * (bottom - top).max(1e-12))).log2();
        let mut camera = Self {
            x: ((left + right) / 2.0).rem_euclid(1.0),
            y: (top + bottom) / 2.0,
            zoom: zoom_x.min(zoom_y).clamp(MIN_ZOOM, MAX_ZOOM),
        };
        camera.constrain_to_viewport(height);
        camera
    }

    /// Match MapLibre's default HeightOnly constraint before unprojecting hover
    /// positions; otherwise native rendering silently uses a different camera.
    pub fn constrain_to_viewport(&mut self, height: f64) {
        let height = height.max(1.0);
        self.zoom = self.zoom.max((height / 512.0).log2());
        let half_height = (height / self.world_size() / 2.0).min(0.5);
        self.y = self.y.clamp(half_height, 1.0 - half_height);
    }

    pub fn update(self) -> CameraUpdate {
        CameraUpdate::new()
            .center(LatLng {
                lat: (PI * (1.0 - 2.0 * self.y)).sinh().atan().to_degrees(),
                lng: self.x * 360.0 - 180.0,
            })
            .zoom(self.zoom)
    }

    pub fn pan(&mut self, dx: f64, dy: f64) {
        let world_size = self.world_size();
        self.x = (self.x - dx / world_size).rem_euclid(1.0);
        self.y = (self.y - dy / world_size).clamp(0.0, 1.0);
    }

    /// `offset` is the pointer's logical-pixel offset from the viewport center.
    pub fn zoom_at(&mut self, delta: f64, offset: (f64, f64)) {
        let old_size = self.world_size();
        self.zoom = (self.zoom + delta).clamp(MIN_ZOOM, MAX_ZOOM);
        let new_size = self.world_size();
        self.x = (self.x + offset.0 / old_size - offset.0 / new_size).rem_euclid(1.0);
        self.y = (self.y + offset.1 / old_size - offset.1 / new_size).clamp(0.0, 1.0);
    }

    fn world_size(self) -> f64 {
        512.0 * self.zoom.exp2()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coordinate_under_pointer(camera: MapCamera, offset: (f64, f64)) -> (f64, f64) {
        (camera.x + offset.0 / camera.world_size(), camera.y + offset.1 / camera.world_size())
    }

    #[test]
    fn zoom_keeps_the_geographic_point_under_the_pointer() {
        let mut camera = MapCamera::default();
        let offset = (170.0, -100.0);
        let before = coordinate_under_pointer(camera, offset);
        camera.zoom_at(2.0, offset);
        let after = coordinate_under_pointer(camera, offset);
        assert!((before.0 - after.0).abs() < 1e-12 && (before.1 - after.1).abs() < 1e-12);
    }

    #[test]
    fn cursor_coordinates_follow_navigation_and_wrap_longitude() {
        let mut camera = MapCamera { x: 0.5, y: 0.5, zoom: 0.0 };
        assert_eq!(camera.coordinate_at((0.0, 0.0)), geo::Coordinate::latlon(0.0, 0.0));
        assert_eq!(camera.coordinate_at((128.0, 0.0)).longitude, 90.0);
        assert_eq!(camera.coordinate_at((384.0, 0.0)).longitude, -90.0);
        assert!(camera.coordinate_at((0.0, -128.0)).latitude > 60.0);
        let offset = (100.0, -70.0);
        let before = camera.coordinate_at(offset);
        camera.zoom_at(2.0, offset);
        let after = camera.coordinate_at(offset);
        assert!((before.latitude - after.latitude).abs() < 1e-12 && (before.longitude - after.longitude).abs() < 1e-12);
        camera.pan(40.0, 20.0);
        let moved = camera.coordinate_at((offset.0 + 40.0, offset.1 + 20.0));
        assert!((before.latitude - moved.latitude).abs() < 1e-12 && (before.longitude - moved.longitude).abs() < 1e-12);
    }

    #[test]
    fn dragging_moves_a_geographic_point_with_the_pointer() {
        let mut camera = MapCamera::default();
        let before = coordinate_under_pointer(camera, (0.0, 0.0));
        camera.pan(100.0, -50.0);
        let after = coordinate_under_pointer(camera, (100.0, -50.0));
        assert!((before.0 - after.0).abs() < 1e-12 && (before.1 - after.1).abs() < 1e-12);
    }

    #[test]
    fn navigation_wraps_longitude_and_clamps_poles_and_zoom() {
        let mut camera = MapCamera::default();
        camera.pan(100_000.0, 100_000.0);
        camera.zoom_at(100.0, (0.0, 0.0));
        assert!((0.0..1.0).contains(&camera.x) && camera.y == 0.0 && camera.zoom == MAX_ZOOM);
        camera.zoom_at(-100.0, (0.0, 0.0));
        assert_eq!(camera.zoom, MIN_ZOOM);
    }

    #[test]
    fn intersection_includes_contained_and_partially_visible_bounds() {
        let camera = MapCamera { x: 0.5, y: 0.5, zoom: 0.0 };
        for bounds in [[-10.0, -10.0, 10.0, 10.0], [30.0, -10.0, 60.0, 10.0], [-10.0, 30.0, 10.0, 60.0]] {
            assert!(camera.intersects_bounds(bounds, 128.0, 128.0), "bounds: {bounds:?}");
        }
    }

    #[test]
    fn intersection_includes_bounds_spanning_viewport_with_all_corners_outside() {
        let camera = MapCamera { x: 0.5, y: 0.5, zoom: 0.0 };
        for bounds in [[-80.0, -10.0, 80.0, 10.0], [-10.0, -60.0, 10.0, 60.0], [-80.0, -60.0, 80.0, 60.0]] {
            assert!(camera.intersects_bounds(bounds, 128.0, 128.0), "bounds: {bounds:?}");
        }
    }

    #[test]
    fn intersection_rejects_bounds_separated_on_either_axis() {
        let camera = MapCamera { x: 0.5, y: 0.5, zoom: 0.0 };
        for bounds in [
            [60.0, -10.0, 80.0, 10.0],
            [-80.0, -10.0, -60.0, 10.0],
            [-10.0, 50.0, 10.0, 60.0],
            [-10.0, -60.0, 10.0, -50.0],
        ] {
            assert!(!camera.intersects_bounds(bounds, 128.0, 128.0), "bounds: {bounds:?}");
        }
    }

    #[test]
    fn intersection_rejects_touching_only_edges_and_boundary_points() {
        let camera = MapCamera { x: 0.5, y: 0.5, zoom: 0.0 };
        let south = camera.coordinate_at((0.0, 64.0)).latitude;
        let north = camera.coordinate_at((0.0, -64.0)).latitude;
        for bounds in [
            [45.0, -10.0, 60.0, 10.0],
            [-60.0, -10.0, -45.0, 10.0],
            [-10.0, north, 10.0, 60.0],
            [-10.0, -60.0, 10.0, south],
            [45.0, 0.0, 45.0, 0.0],
        ] {
            assert!(!camera.intersects_bounds(bounds, 128.0, 128.0), "bounds: {bounds:?}");
        }
    }

    #[test]
    fn intersection_includes_point_bounds_inside_viewport() {
        let camera = MapCamera { x: 0.5, y: 0.5, zoom: 0.0 };
        assert!(camera.intersects_bounds([0.0, 0.0, 0.0, 0.0], 128.0, 128.0));
    }

    #[test]
    fn intersection_finds_visible_world_copies_on_both_sides_of_dateline() {
        for (x, bounds) in [(0.01, [175.0, -10.0, 179.0, 10.0]), (0.99, [-179.0, -10.0, -175.0, 10.0])] {
            let camera = MapCamera { x, y: 0.5, zoom: 0.0 };
            for shift in [-720.0, 0.0, 720.0] {
                let shifted = [bounds[0] + shift, bounds[1], bounds[2] + shift, bounds[3]];
                assert!(camera.intersects_bounds(shifted, 64.0, 64.0), "camera x: {x}, bounds: {shifted:?}");
            }
        }
    }

    #[test]
    fn intersection_handles_antimeridian_bounds_near_both_world_edges() {
        for x in [0.0, 0.99] {
            let camera = MapCamera { x, y: 0.5, zoom: 0.0 };
            assert!(camera.intersects_bounds([170.0, -10.0, -170.0, 10.0], 64.0, 64.0), "camera x: {x}");
        }
    }

    #[test]
    fn intersection_does_not_treat_antimeridian_bounds_as_crossing_greenwich() {
        let camera = MapCamera { x: 0.5, y: 0.5, zoom: 0.0 };
        assert!(!camera.intersects_bounds([170.0, -10.0, -170.0, 10.0], 64.0, 64.0));
    }

    #[test]
    fn intersection_handles_full_world_and_multiple_world_viewports() {
        let camera = MapCamera { x: 0.2, y: 0.5, zoom: 0.0 };
        for width in [512.0, 768.0, 1536.0] {
            for bounds in [[-179.0, -10.0, -175.0, 10.0], [175.0, -10.0, 179.0, 10.0], [170.0, -10.0, -170.0, 10.0]] {
                assert!(camera.intersects_bounds(bounds, width, 64.0), "width: {width}, bounds: {bounds:?}");
            }
        }
    }

    #[test]
    fn intersection_still_checks_latitude_for_full_world_viewports() {
        let camera = MapCamera { x: 0.2, y: 0.5, zoom: 0.0 };
        assert!(!camera.intersects_bounds([-180.0, 70.0, 180.0, 80.0], 1536.0, 64.0));
    }

    #[test]
    fn intersection_includes_full_world_bounds_in_narrow_viewports() {
        for x in [0.0, 0.2, 0.99] {
            let camera = MapCamera { x, y: 0.5, zoom: 0.0 };
            assert!(camera.intersects_bounds([-180.0, -90.0, 180.0, 90.0], 64.0, 64.0), "camera x: {x}");
        }
    }

    #[test]
    fn intersection_includes_polar_bounds_overlapping_visible_latitudes() {
        for (y, bounds) in [(0.0, [-10.0, 80.0, 10.0, 90.0]), (1.0, [-10.0, -90.0, 10.0, -80.0])] {
            let camera = MapCamera { x: 0.5, y, zoom: 0.0 };
            assert!(camera.intersects_bounds(bounds, 128.0, 128.0), "camera y: {y}");
        }
    }

    #[test]
    fn intersection_rejects_polar_bounds_entirely_beyond_mercator_cap() {
        for (y, bounds) in [(0.0, [-10.0, 86.0, 10.0, 90.0]), (1.0, [-10.0, -90.0, 10.0, -86.0])] {
            let camera = MapCamera { x: 0.5, y, zoom: 0.0 };
            assert!(!camera.intersects_bounds(bounds, 128.0, 128.0), "camera y: {y}");
        }
    }

    #[test]
    fn intersection_does_not_wrap_latitude_at_poles() {
        let camera = MapCamera { x: 0.5, y: 0.0, zoom: 0.0 };
        assert!(!camera.intersects_bounds([-10.0, -90.0, 10.0, -80.0], 128.0, 128.0));
    }

    #[test]
    fn intersection_uses_zoom_scaled_world_size() {
        let camera = MapCamera { x: 0.5, y: 0.5, zoom: 2.0 };
        assert!(!camera.intersects_bounds([30.0, -10.0, 60.0, 10.0], 128.0, 128.0));
    }

    #[test]
    fn intersection_rejects_invalid_bounds_and_viewport_dimensions() {
        let camera = MapCamera::default();
        for (bounds, width, height) in [
            ([-180.0, -90.0, 180.0, 90.0], 0.0, 128.0),
            ([-180.0, -90.0, 180.0, 90.0], 128.0, -1.0),
            ([-180.0, -90.0, 180.0, 90.0], f64::INFINITY, 128.0),
            ([-180.0, -90.0, 180.0, 90.0], 128.0, f64::NAN),
            ([f64::NAN, -90.0, 180.0, 90.0], 128.0, 128.0),
            ([-180.0, 10.0, 180.0, -10.0], 128.0, 128.0),
        ] {
            assert!(
                !camera.intersects_bounds(bounds, width, height),
                "bounds: {bounds:?}, viewport: {width} x {height}"
            );
        }
    }

    #[test]
    fn fitting_bounds_keeps_the_extent_inside_the_viewport() {
        let camera = MapCamera::fit_bounds([2.5, 50.5, 6.0, 51.5], 800.0, 600.0);
        for (longitude, latitude) in [(2.5_f64, 50.5_f64), (6.0, 51.5)] {
            let x = (longitude + 180.0) / 360.0;
            let y = (1.0 - latitude.to_radians().tan().asinh() / PI) / 2.0;
            assert!((x - camera.x).abs() * camera.world_size() < 400.0);
            assert!((y - camera.y).abs() * camera.world_size() < 300.0);
        }
    }

    #[test]
    fn global_fit_uses_the_native_minimum_zoom_for_the_viewport_height() {
        for height in [400.0, 760.0, 1200.0] {
            let camera = MapCamera::fit_bounds([-180.0, -90.0, 180.0, 90.0], 1200.0, height);
            assert!((camera.world_size() - height.max(512.0)).abs() < 1e-9);
            assert!((camera.y - 0.5).abs() < 1e-12);
        }
    }

    #[test]
    fn viewport_constraints_clamp_polar_panning_without_wrapping_latitude() {
        let mut camera = MapCamera { x: 0.2, y: 0.5, zoom: 2.0 };
        for (dy, expected_y) in [(10_000.0, 0.185546875), (-10_000.0, 0.814453125)] {
            camera.pan(0.0, dy);
            camera.constrain_to_viewport(760.0);
            assert_eq!(
                camera,
                MapCamera {
                    x: 0.2,
                    y: expected_y,
                    zoom: 2.0
                }
            );
        }
    }

    #[test]
    fn viewport_constraints_follow_zoom_and_resize_and_are_idempotent() {
        let mut camera = MapCamera::default();
        camera.zoom_at(-100.0, (170.0, 100.0));
        camera.constrain_to_viewport(760.0);
        let constrained = camera;
        camera.constrain_to_viewport(760.0);
        assert_eq!(camera, constrained);
        camera.constrain_to_viewport(1200.0);
        assert!((camera.world_size() - 1200.0).abs() < 1e-9);
        assert_eq!(camera.y, 0.5);
        camera.constrain_to_viewport(400.0);
        assert!((camera.world_size() - 1200.0).abs() < 1e-9, "Shrinking the viewport must not zoom out");
    }

    #[test]
    fn fitting_bounds_handles_polar_and_antimeridian_extents() {
        let polar = MapCamera::fit_bounds([-180.0, -90.0, 180.0, 90.0], 800.0, 600.0);
        assert!(polar.zoom.is_finite() && polar.y.is_finite());
        let dateline = MapCamera::fit_bounds([170.0, -10.0, -170.0, 10.0], 800.0, 600.0);
        assert!(dateline.x < 1e-10 && dateline.zoom > 3.0);
    }
}
