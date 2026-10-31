use super::{MapCamera, mercator_y, projected_ground_bounds};

fn camera(pitch: f64, bearing: f64) -> MapCamera {
    MapCamera {
        x: 0.5,
        y: 0.5,
        zoom: 4.0,
        pitch,
        bearing,
        viewport_height: 600.0,
    }
}

fn assert_coordinate_eq(a: geo::Coordinate, b: geo::Coordinate) {
    assert!((a.latitude - b.latitude).abs() < 1e-9, "latitude: {a:?} != {b:?}");
    assert!((a.longitude - b.longitude).abs() < 1e-9, "longitude: {a:?} != {b:?}");
}

// Independent forward transform: rotate the ground plane, pitch it about the
// center, translate the eye by 1.5 viewport heights, then perspective-divide.
fn project(camera: MapCamera, point: geo::Coordinate) -> (f64, f64) {
    let x = ((point.longitude + 180.0) / 360.0 - camera.x) * camera.world_size();
    let y = (mercator_y(point.latitude) - camera.y) * camera.world_size();
    let (sin_b, cos_b) = camera.bearing.to_radians().sin_cos();
    let (sin_p, cos_p) = camera.pitch.to_radians().sin_cos();
    let rotated_x = cos_b * x + sin_b * y;
    let rotated_y = -sin_b * x + cos_b * y;
    let distance = camera.viewport_height * 1.5;
    let scale = distance / (distance - rotated_y * sin_p);
    (rotated_x * scale, rotated_y * cos_p * scale)
}

#[test]
fn unprojection_round_trips_perspective_and_bearing_at_all_viewport_corners() {
    for pitch in [0.0, 30.0, 60.0] {
        for bearing in [0.0, 90.0, 180.0, 270.0, 37.0] {
            let camera = camera(pitch, bearing);
            for offset in [(-400.0, -300.0), (400.0, -300.0), (400.0, 300.0), (-400.0, 300.0), (100.0, -120.0)] {
                let actual = project(camera, camera.coordinate_at(offset));
                assert!(
                    (actual.0 - offset.0).abs() < 1e-8 && (actual.1 - offset.1).abs() < 1e-8,
                    "{camera:?}: {actual:?} != {offset:?}"
                );
            }
        }
    }
}

#[test]
fn east_bearing_places_east_at_top_and_pitch_expands_far_ground() {
    let east_up = camera(0.0, 90.0).coordinate_at((0.0, -100.0));
    assert!(east_up.longitude > 0.0 && east_up.latitude.abs() < 1e-9);
    let pitched = camera(60.0, 0.0);
    let north = pitched.coordinate_at((0.0, -300.0));
    let south = pitched.coordinate_at((0.0, 300.0));
    assert!(north.latitude > -south.latitude * 2.0);
}

#[test]
fn off_center_drag_and_zoom_preserve_the_ground_anchor() {
    for bearing in [0.0, 90.0, 37.0] {
        let mut camera = camera(60.0, bearing);
        let from = (150.0, -200.0);
        let to = (210.0, -130.0);
        let anchor = camera.coordinate_at(from);
        camera.pan_between(from, to);
        assert_coordinate_eq(camera.coordinate_at(to), anchor);
        camera.zoom_at(1.25, to);
        assert_coordinate_eq(camera.coordinate_at(to), anchor);
        camera.zoom_at(-0.75, to);
        assert_coordinate_eq(camera.coordinate_at(to), anchor);
    }
}

#[test]
fn viewport_height_changes_perspective_but_dpi_does_not_enter_logical_coordinates() {
    let mut camera = camera(60.0, 37.0);
    let before = camera.coordinate_at((100.0, -150.0));
    camera.constrain_to_viewport(1200.0);
    assert_ne!(camera.coordinate_at((100.0, -150.0)), before);
    camera.zoom_at(1.0, (0.0, 0.0));
    assert_coordinate_eq(camera.coordinate_at((200.0, -300.0)), before);
}

#[test]
fn visible_bounds_include_far_corners_and_unwrapped_dateline_worlds() {
    let mut camera = camera(60.0, 37.0);
    camera.x = 0.99;
    let [left, top, right, bottom] = camera.visible_region(800.0, 600.0);
    assert!(right > 1.0);
    for offset in [(-400.0, -300.0), (400.0, -300.0), (400.0, 300.0), (-400.0, 300.0)] {
        let (dx, dy) = camera.ground_offset(offset, 600.0);
        let (x, y) = (camera.x + dx / camera.world_size(), camera.y + dy / camera.world_size());
        assert!(x >= left && x <= right && y >= top && y <= bottom);
        let coordinate = camera.coordinate_at((offset.0 * 0.99, offset.1 * 0.99));
        assert!(camera.intersects_bounds(
            [coordinate.longitude, coordinate.latitude, coordinate.longitude, coordinate.latitude],
            800.0,
            600.0
        ));
    }
}

fn projection(camera: MapCamera, width: f64) -> [f64; 16] {
    let (sin_b, cos_b) = camera.bearing.to_radians().sin_cos();
    let (sin_p, cos_p) = camera.pitch.to_radians().sin_cos();
    let distance = camera.viewport_height * 1.5;
    let sx = 2.0 * distance / width;
    let sy = -2.0 * distance * cos_p / camera.viewport_height;
    let mut matrix = [0.0; 16];
    matrix[0] = sx * cos_b;
    matrix[4] = sx * sin_b;
    matrix[1] = -sy * sin_b;
    matrix[5] = sy * cos_b;
    matrix[3] = sin_p * sin_b;
    matrix[7] = -sin_p * cos_b;
    let (x, y) = (camera.x * camera.world_size(), camera.y * camera.world_size());
    matrix[12] = -matrix[0] * x - matrix[4] * y;
    matrix[13] = -matrix[1] * x - matrix[5] * y;
    matrix[15] = distance - matrix[3] * x - matrix[7] * y;
    matrix
}

#[test]
fn matrix_ground_coverage_matches_camera_for_pitch_bearing_and_world_copies() {
    for pitch in [0.0, 30.0, 60.0] {
        for bearing in [0.0, 90.0, 37.0, 270.0] {
            let mut camera = camera(pitch, bearing);
            camera.x = 1.01;
            let actual = projected_ground_bounds(&projection(camera, 800.0), camera.zoom).unwrap();
            let expected = camera.visible_region(800.0, 600.0);
            for (actual, expected) in actual.into_iter().zip(expected) {
                assert!((actual - expected).abs() < 1e-12, "{camera:?}: {actual} != {expected}");
            }
        }
    }
    assert!(projected_ground_bounds(&[0.0; 16], 4.0).is_none());
    assert!(projected_ground_bounds(&[f64::NAN; 16], 4.0).is_none());
}
