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
        Self {
            x: ((left + right) / 2.0).rem_euclid(1.0),
            y: (top + bottom) / 2.0,
            zoom: zoom_x.min(zoom_y).clamp(MIN_ZOOM, MAX_ZOOM),
        }
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
    fn fitting_bounds_handles_polar_and_antimeridian_extents() {
        let polar = MapCamera::fit_bounds([-180.0, -90.0, 180.0, 90.0], 800.0, 600.0);
        assert!(polar.zoom.is_finite() && polar.y.is_finite());
        let dateline = MapCamera::fit_bounds([170.0, -10.0, -170.0, 10.0], 800.0, 600.0);
        assert!(dateline.x < 1e-10 && dateline.zoom > 3.0);
    }
}
