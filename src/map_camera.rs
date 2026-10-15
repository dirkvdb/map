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
}
