use std::collections::HashSet;

use super::*;

fn parameters() -> WgpuLayerParameters {
    let mut parameters = tests::coverage_parameters();
    parameters.globe = true;
    parameters.projection_transition = 1.0;
    // Orthographic native sphere convention, looking toward (0°, 0°).
    parameters.globe_projection = [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0];
    parameters.globe_clipping_plane = [0.0, 0.0, 1.0, 0.0];
    parameters
}

fn world_bounds() -> [LatLonBounds; 1] {
    [[-180.0, -85.0511287798066, 180.0, 85.0511287798066].into()]
}

#[test]
fn globe_rejects_backside_but_keeps_horizon_crossing_tiles() {
    let parameters = parameters();
    assert!(!globe_tile_visible(&parameters, VisibleTile { z: 6, x: 0, y: 32 }));
    assert!(globe_tile_visible(&parameters, VisibleTile { z: 6, x: 31, y: 32 }));
    assert!(globe_tile_visible(&parameters, VisibleTile { z: 6, x: 15, y: 32 }));
}

#[test]
fn globe_depth_offset_preserves_visible_sliver_past_horizon() {
    let mut parameters = parameters();
    let tile = VisibleTile { z: 6, x: 14, y: 32 };
    assert!(!globe_tile_visible(&parameters, tile));
    parameters.depth_offset = 0.2;
    assert!(globe_tile_visible(&parameters, tile));
}

#[test]
fn globe_blend_does_not_hard_cull_backside_or_use_flat_ground_bounds() {
    let mut parameters = parameters();
    parameters.projection_transition = 0.5;
    // The existing world-pixel projection misses the globe completely.
    parameters.projection[12] = 100.0;
    parameters.projection[15] = 1.0;
    let backside = VisibleTile { z: 6, x: 0, y: 32 };
    assert!(tiles_at_zoom(&parameters, 6, &world_bounds()).contains(&backside));
}

#[test]
fn globe_blend_culls_using_blended_xyw_not_either_endpoint() {
    let mut parameters = parameters();
    parameters.projection_transition = 0.5;
    parameters.globe_projection[12] = 3.0;
    parameters.mercator_projection[12] = -3.0;
    parameters.mercator_projection[15] = 1.0;
    assert!(globe_tile_visible(&parameters, VisibleTile { z: 6, x: 31, y: 32 }));
}

#[test]
fn globe_rejects_tiles_outside_native_globe_frustum() {
    let mut parameters = parameters();
    parameters.globe_projection[12] = 4.0;
    assert!(tiles_at_zoom(&parameters, 6, &world_bounds()).is_empty());
}

#[test]
fn globe_antimeridian_coverage_is_canonical_and_respects_source_bounds() {
    let mut parameters = parameters();
    parameters.longitude = 540.0;
    parameters.globe_clipping_plane = [0.0, 0.0, -1.0, 0.0];
    let bounds = [[170.0, -10.0, -170.0, 10.0].into()];
    let tiles = tiles_at_zoom(&parameters, 6, &bounds);
    assert!(!tiles.is_empty());
    assert!(tiles.iter().any(|tile| tile.x == 0));
    assert!(tiles.iter().any(|tile| tile.x == 63));
    assert!(tiles.iter().all(|tile| (0..64).contains(&tile.x) && intersects_bounds(*tile, bounds[0])));
    assert_eq!(tiles.len(), tiles.iter().map(|tile| tile.canonical()).collect::<HashSet<_>>().len());
    parameters.longitude = -180.0;
    assert_eq!(tiles, tiles_at_zoom(&parameters, 6, &bounds));
}

fn dateline_blend_parameters(longitude: f64) -> WgpuLayerParameters {
    let mut parameters = parameters();
    parameters.zoom = 11.5;
    parameters.longitude = longitude;
    parameters.projection_transition = 0.5;
    let center = longitude / 360.0 + 0.5;
    let scale = 2.0 * parameters.zoom.exp2();
    parameters.mercator_projection[0] = scale;
    parameters.mercator_projection[5] = scale;
    parameters.mercator_projection[12] = -scale * center;
    parameters.mercator_projection[13] = -scale * 0.5;
    parameters.mercator_projection[15] = 1.0;
    // Native-like local tangent projection with the same pixels/world scale.
    let angle = longitude.to_radians();
    parameters.globe_projection[0] = angle.cos() * scale / std::f64::consts::TAU;
    parameters.globe_projection[5] = scale / std::f64::consts::TAU;
    parameters.globe_projection[8] = -angle.sin() * scale / std::f64::consts::TAU;
    parameters.globe_clipping_plane = [angle.sin(), 0.0, angle.cos(), 0.0];
    parameters
}

#[test]
fn globe_blend_keeps_both_dateline_sides_in_camera_nearest_world() {
    let bounds = [[179.8, -0.1, -179.8, 0.1].into()];
    for longitude in [179.99, -179.99, 539.99, -539.99] {
        let parameters = dateline_blend_parameters(longitude);
        let tiles = tiles_at_zoom(&parameters, 12, &bounds);
        let seam = ((longitude / 360.0 + 0.5).round() * 4096.0) as i64;
        assert!(tiles.contains(&VisibleTile { z: 12, x: seam - 1, y: 2048 }), "west seam missing at {longitude}");
        assert!(tiles.contains(&VisibleTile { z: 12, x: seam, y: 2048 }), "east seam missing at {longitude}");
        assert_eq!(tiles.len(), tiles.iter().map(|tile| tile.canonical()).collect::<HashSet<_>>().len());
        assert!(tiles.iter().all(|tile| {
            let center = longitude / 360.0 + 0.5;
            ((tile.x as f64 + 0.5) / 4096.0 - center).abs() <= 0.5
        }));
    }
}

#[test]
fn globe_blend_ancestor_ranges_enclose_descendants_with_different_wraps() {
    let parameters = dateline_blend_parameters(180.0);
    let parent = VisibleTile { z: 0, x: 0, y: 0 };
    let child = VisibleTile { z: 1, x: 0, y: 0 };
    let parent_range = globe_mercator_ranges(&parameters, parent)[0];
    let child_range = globe_mercator_ranges(&parameters, child)[0];
    assert!(parent_range[0] <= child_range[0] && parent_range[1] >= child_range[1]);
}

#[test]
fn globe_blend_extrusion_coverage_uses_same_nearest_wrap_without_duplicates() {
    let parameters = dateline_blend_parameters(540.0);
    let tiles = globe_tiles(&parameters, 6, &[[170.0, -10.0, -170.0, 10.0].into()], 64, false);
    assert!(tiles.iter().any(|tile| tile.x == 127));
    assert!(tiles.iter().any(|tile| tile.x == 128));
    assert_eq!(tiles.len(), tiles.iter().map(|tile| tile.canonical()).collect::<HashSet<_>>().len());
}

#[test]
fn globe_snap_to_full_projection_keeps_canonical_output() {
    let mut parameters = dateline_blend_parameters(540.0);
    parameters.projection_transition = 0.9991;
    let tiles = tiles_at_zoom(&parameters, 6, &[[170.0, -10.0, -170.0, 10.0].into()]);
    assert!(!tiles.is_empty());
    assert!(tiles.iter().all(|tile| (0..64).contains(&tile.x)));
}

#[test]
fn globe_high_zoom_coverage_coarsens_without_leaving_holes() {
    let parameters = parameters();
    let tiles = globe_tiles(&parameters, 22, &world_bounds(), 64, true);
    assert!(!tiles.is_empty() && tiles.len() <= 64);
    let z = tiles[0].z;
    assert!(tiles.iter().all(|tile| tile.z == z));
    assert_eq!(tiles, globe_tiles(&parameters, z, &world_bounds(), 64, true));
}

#[test]
fn globe_small_high_zoom_view_keeps_native_tile_resolution() {
    let mut parameters = parameters();
    parameters.zoom = 21.0;
    parameters.globe_projection[0] = 1_000_000.0;
    parameters.globe_projection[5] = 1_000_000.0;
    let tiles = globe_tiles(&parameters, 22, &world_bounds(), 4096, true);
    assert!(!tiles.is_empty());
    assert!(tiles.iter().all(|tile| tile.z == 22));
}

#[test]
fn globe_root_keeps_visible_interior_when_all_four_corners_are_behind_horizon() {
    assert!(globe_tile_visible(&parameters(), VisibleTile { z: 0, x: 0, y: 0 }));
}

#[test]
fn globe_invalid_projection_fails_open_with_bounded_coverage() {
    let mut parameters = parameters();
    parameters.globe_projection = [f64::NAN; 16];
    let tiles = globe_tiles(&parameters, 22, &world_bounds(), 16, true);
    assert_eq!(tiles.len(), 16);
}

#[test]
fn globe_surface_ranges_enclose_curved_tile_edges_and_interior() {
    for (z, x, y) in [(0, 0, 0), (1, 0, 0), (3, 7, 0), (6, 15, 32), (22, 1_234_567, 2_000_000)] {
        let tile = VisibleTile { z, x, y };
        let ranges = sphere_ranges(tile);
        let bounds = tile.canonical().bounds();
        for x in 0..=16 {
            for y in 0..=16 {
                let longitude = (bounds.west() + (bounds.east() - bounds.west()) * f64::from(x) / 16.0).to_radians();
                let latitude = (bounds.south() + (bounds.north() - bounds.south()) * f64::from(y) / 16.0).to_radians();
                let sphere = [longitude.sin() * latitude.cos(), latitude.sin(), longitude.cos() * latitude.cos()];
                assert!(
                    sphere
                        .iter()
                        .zip(ranges)
                        .all(|(value, [low, high])| *value >= low - 1e-12 && *value <= high + 1e-12)
                );
            }
        }
    }
}

#[test]
fn globe_extrusion_retained_tiles_fail_open_even_on_backside() {
    let parameters = parameters();
    assert!(extrusion_view::tile_visible(&parameters, VisibleTile { z: 6, x: 0, y: 32 }, 1000.0));
}

#[test]
fn globe_extrusion_coverage_retains_raised_tiles_outside_surface_frustum() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = tests::coverage_source(&dir.path().join("globe-extrusion.tif"), [-10.0, -10.0, 10.0, 10.0])?;
    let mut parameters = parameters();
    parameters.globe_projection[12] = 4.0;
    let tiles = extrusion_view::visible_tiles(&parameters, &[(&source, 1000.0)], 16);
    assert!(!tiles.is_empty() && tiles.len() <= 16);
    Ok(())
}

#[test]
fn inactive_globe_retains_mercator_world_wraps_and_zoom() {
    let mut parameters = parameters();
    parameters.projection_transition = 0.0;
    parameters.longitude = 360.0;
    let tiles = tiles_at_zoom(&parameters, 2, &world_bounds());
    assert!(tiles.iter().all(|tile| tile.z == 2 && (4..8).contains(&tile.x)));
    parameters.globe = false;
    parameters.projection_transition = 1.0;
    assert_eq!(tiles, tiles_at_zoom(&parameters, 2, &world_bounds()));
}
