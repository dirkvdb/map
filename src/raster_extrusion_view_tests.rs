use std::collections::HashSet;

use anyhow::Result;

use super::*;
use crate::raster_tiles::{test_support::parameters, tests::coverage_source};

fn orthographic(zoom: f64, longitude: f64, latitude: f64, half_span: f64, bearing: f64) -> WgpuLayerParameters {
    let mut p = parameters(zoom);
    p.longitude = longitude;
    p.latitude = latitude;
    let origin = camera_origin(&p);
    let world = 512.0 * zoom.exp2();
    let (sin, cos) = bearing.to_radians().sin_cos();
    p.projection = [0.0; 16];
    p.projection[0] = cos / half_span;
    p.projection[4] = sin / half_span;
    p.projection[1] = sin / half_span;
    p.projection[5] = -cos / half_span;
    p.projection[9] = 0.0001;
    p.projection[10] = -0.000000001;
    p.projection[12] = -world * (p.projection[0] * origin[0] + p.projection[4] * origin[1]);
    p.projection[13] = -world * (p.projection[1] * origin[0] + p.projection[5] * origin[1]);
    p.projection[14] = 0.5;
    p.projection[15] = 1.0;
    p
}

#[test]
fn diagonal_frustum_rejects_tiles_in_ground_bbox_corners() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let wide = coverage_source(&directory.path().join("wide.tif"), [-40.0, -40.0, 40.0, 40.0])?;
    let fine = coverage_source(&directory.path().join("fine.tif"), [-0.05, -0.05, 0.05, 0.05])?;
    let p = orthographic(5.0, 0.0, 0.0, 400.0, 45.0);
    let corner = VisibleTile { z: 6, x: 34, y: 34 };
    let flat = super::super::tiles_at_zoom(&p, 6, &[wide.metadata().bounds.into()]);
    let tiles = visible_tiles(&p, &[(&wide, 0.001), (&fine, 0.001)], 1024);
    assert!(flat.contains(&corner), "Regression fixture must be inside the old broad phase");
    assert!(!tiles.contains(&corner));
    assert!(!tiles.is_empty());
    assert!(tiles.len() < flat.len());
    assert!(tiles.iter().all(|tile| tile.z == 6));
    Ok(())
}

#[test]
fn elevated_bars_are_generated_even_when_their_ground_is_offscreen() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = coverage_source(&directory.path().join("bars.tif"), [-1.0, -1.0, 1.0, 1.0])?;
    let p = orthographic(5.0, 0.0, 5.0, 64.0, 0.0);
    let sources = [(&*source, 2.0)];
    let z = selected_zoom(&p, &source);
    let bar = VisibleTile {
        z,
        x: 1 << (z - 1),
        y: 1 << (z - 1),
    };
    let height = maximum_height(&p, &sources);
    assert!(!super::super::visible_tiles(&p, &source).contains(&bar));
    assert!(!tile_visible(&p, bar, 0.0));
    assert!(tile_visible(&p, bar, height));
    assert!(visible_tiles(&p, &sources, 256).contains(&bar));
    Ok(())
}

#[test]
fn zero_projection_preserves_bounded_normal_coverage() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = coverage_source(&directory.path().join("fallback.tif"), [-40.0, -20.0, 40.0, 20.0])?;
    let mut p = parameters(2.0);
    p.projection = [0.0; 16];
    p.width = 512.0;
    p.height = 512.0;
    let normal = super::super::visible_tiles(&p, &source);
    assert_eq!(visible_tiles(&p, &[(&source, 1.0)], 256), normal);
    assert!(normal.iter().all(|tile| tile_visible(&p, *tile, 10_000.0)));
    Ok(())
}

#[test]
fn disabled_sources_do_not_change_the_normal_api() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = coverage_source(&directory.path().join("disabled.tif"), [-1.0, -1.0, 1.0, 1.0])?;
    let p = orthographic(5.0, 0.0, 5.0, 64.0, 0.0);
    let before = super::super::visible_tiles(&p, &source);
    let _ = visible_tiles(&p, &[(&source, 0.0)], 256);
    assert_eq!(super::super::visible_tiles(&p, &source), before);
    assert_eq!(maximum_height(&p, &[(&source, -1.0), (&source, 0.0)]), 0.0);
    assert!(visible_tiles(&p, &[], 256).is_empty());
    Ok(())
}

#[test]
fn coverage_keeps_unwrapped_copies_and_camera_order() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let west = coverage_source(&directory.path().join("west.tif"), [-179.0, -5.0, -170.0, 5.0])?;
    let east = coverage_source(&directory.path().join("east.tif"), [170.0, -5.0, 179.0, 5.0])?;
    let p = orthographic(2.0, 180.0, 0.0, 1400.0, 0.0);
    let sources = [(&*west, 0.01), (&*east, 0.01)];
    let tiles = visible_tiles(&p, &sources, 256);
    let wrapped = visible_tiles(&orthographic(2.0, 540.0, 0.0, 1400.0, 0.0), &sources, 256);
    let expected: Vec<_> = tiles
        .iter()
        .map(|tile| VisibleTile {
            x: tile.x + (1 << tile.z),
            ..*tile
        })
        .collect();
    assert!(!tiles.is_empty());
    assert!(tiles.iter().any(|tile| tile.x >= 1 << tile.z));
    assert_eq!(wrapped, expected);
    assert!(tiles.iter().zip(&wrapped).all(|(a, b)| a.canonical() == b.canonical()));
    let distance = |tile: VisibleTile| (tile.x as f64 + 0.5 - f64::from(1 << tile.z)).hypot(f64::from(tile.y) + 0.5 - f64::from(1 << tile.z) / 2.0);
    assert!(tiles.windows(2).all(|pair| distance(pair[0]) <= distance(pair[1])));
    Ok(())
}

#[test]
fn antimeridian_bounds_filter_canonical_tiles_but_keep_copies() {
    let p = orthographic(2.0, 180.0, 0.0, 2200.0, 0.0);
    let view = View::new(&p, 1000.0);
    let bounds: LatLonBounds = [170.0, -10.0, -170.0, 10.0].into();
    let tiles = view.coverage(3, &[bounds], 256).unwrap();
    assert!(!tiles.is_empty());
    assert!(tiles.iter().all(|tile| [0, 7].contains(&tile.canonical().x)));
    assert!(tiles.iter().any(|tile| tile.x < 0));
    assert!(tiles.iter().any(|tile| tile.x >= 8));
}

#[test]
fn canonical_limit_coarsens_without_counting_wrapped_copies() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let wide = coverage_source(&directory.path().join("wide.tif"), [-170.0, -70.0, 170.0, 70.0])?;
    let fine = coverage_source(&directory.path().join("fine.tif"), [-0.05, -0.05, 0.05, 0.05])?;
    let p = orthographic(5.0, 0.0, 0.0, 18000.0, 35.0);
    let sources = [(&*wide, 1.0), (&*fine, 2.0)];
    let height = maximum_height(&p, &sources);
    let tiles = visible_tiles(&p, &sources, 8);
    let unique: HashSet<_> = tiles.iter().map(|tile| tile.canonical()).collect();
    assert!(!tiles.is_empty());
    assert!(unique.len() <= 8);
    assert!(tiles.len() > unique.len(), "Copies are distinct draws, not extra requests");
    assert!(tiles.iter().all(|tile| tile.z == tiles[0].z && tile.z < 6));
    let bounds = [wide.metadata().bounds.into(), fine.metadata().bounds.into()];
    let expected = View::new(&p, height).coverage(tiles[0].z, &bounds, usize::MAX).unwrap();
    assert_eq!(tiles.iter().copied().collect::<HashSet<_>>(), expected.into_iter().collect());
    assert_eq!(height, maximum_height(&p, &sources));
    Ok(())
}

#[test]
fn coarsening_preserves_offscreen_ground_bars_without_changing_the_height_slab() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let south = coverage_source(&directory.path().join("south.tif"), [-10.0, -1.0, 10.0, 1.0])?;
    let north = coverage_source(&directory.path().join("north.tif"), [-10.0, 4.0, 10.0, 6.0])?;
    let fine = coverage_source(&directory.path().join("fine.tif"), [-0.05, -0.05, 0.05, 0.05])?;
    let mut p = orthographic(5.0, 0.0, 5.0, 64.0, 0.0);
    p.projection[0] = 1.0 / 400.0;
    p.projection[12] = -512.0 * p.zoom.exp2() * 0.5 / 400.0;
    let sources = [(&*south, 0.8), (&*north, 0.0), (&*fine, 0.8)];
    let height = maximum_height(&p, &sources);
    let detailed = visible_tiles(&p, &sources, 256);
    let coarsened = visible_tiles(&p, &sources, 4);
    let bar = VisibleTile { z: 6, x: 32, y: 32 };
    let parent = VisibleTile { z: 5, x: 16, y: 16 };
    assert!(detailed.contains(&bar));
    assert!(coarsened.iter().all(|tile| tile.z == 5));
    assert!(coarsened.contains(&parent));
    assert!(!tile_visible(&p, bar, 0.0));
    assert!(tile_visible(&p, bar, height));
    assert!(tile_visible(&p, parent, height));
    let bounds = [south.metadata().bounds.into(), north.metadata().bounds.into(), fine.metadata().bounds.into()];
    let expected = View::new(&p, height).coverage(5, &bounds, usize::MAX).unwrap();
    assert_eq!(coarsened.into_iter().collect::<HashSet<_>>(), expected.into_iter().collect());
    Ok(())
}

#[test]
fn zero_limit_terminates_at_zoom_zero() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = coverage_source(&directory.path().join("zero-limit.tif"), [-1.0, -1.0, 1.0, 1.0])?;
    let p = orthographic(5.0, 0.0, 0.0, 64.0, 0.0);
    let tiles = visible_tiles(&p, &[(&source, 1.0)], 0);
    assert!(!tiles.is_empty());
    assert!(tiles.iter().all(|tile| tile.z == 0));
    Ok(())
}

#[test]
fn height_union_uses_each_sources_own_selected_zoom_before_coverage_budgeting() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let coarse = coverage_source(&directory.path().join("coarse.tif"), [-40.0, -20.0, 40.0, 20.0])?;
    let fine = coverage_source(&directory.path().join("fine.tif"), [-0.05, -0.05, 0.05, 0.05])?;
    let p = orthographic(5.0, 0.0, 60.0, 64.0, 0.0);
    let coarse_height = extrusion_height::maximum(&p, &coarse, 1.0);
    let fine_height = extrusion_height::maximum(&p, &fine, 1.0);
    assert!(selected_zoom(&p, &coarse) < selected_zoom(&p, &fine));
    assert!(coarse_height > fine_height);
    assert_eq!(maximum_height(&p, &[(&coarse, 1.0), (&fine, 1.0)]), coarse_height);
    assert_eq!(maximum_height(&p, &[(&coarse, 0.0), (&fine, 1.0)]), fine_height);
    Ok(())
}

#[test]
fn a_coarser_budget_does_not_establish_a_persistent_coarse_zoom_floor() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = coverage_source(&directory.path().join("ancestor.tif"), [-1.0, -1.0, 1.0, 1.0])?;
    let p = orthographic(5.0, 0.0, 0.0, 64.0, 0.0);
    let sources = [(&*source, 0.2)];
    let height = maximum_height(&p, &sources);
    let detailed = visible_tiles(&p, &sources, 256);
    let coarsened = visible_tiles(&p, &sources, 1);
    assert!(detailed.iter().all(|tile| tile.z == selected_zoom(&p, &source)));
    assert!(coarsened.iter().all(|tile| tile.z == 0));
    assert_eq!(maximum_height(&p, &sources), height);
    assert_eq!(visible_tiles(&p, &sources, 256), detailed);
    Ok(())
}

#[test]
fn fade_coverage_unions_both_geographies_and_takes_the_larger_height() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let south = coverage_source(&directory.path().join("south.tif"), [-1.0, -1.0, 1.0, 1.0])?;
    let north = coverage_source(&directory.path().join("north.tif"), [-1.0, 4.0, 1.0, 6.0])?;
    let p = orthographic(5.0, 0.0, 5.0, 64.0, 0.0);
    let sources = [(&*south, 2.0), (&*north, 0.0)];
    let tiles = visible_tiles(&p, &sources, 256);
    let south_only = visible_tiles(&p, &[(&south, 2.0)], 256);
    let north_only = visible_tiles(&p, &[(&north, 0.0)], 256);
    assert!(south_only.iter().chain(&north_only).all(|tile| tiles.contains(tile)));
    assert!(tiles.len() > south_only.len());
    assert_eq!(tiles, visible_tiles(&p, &[(&north, 0.0), (&south, 2.0)], 256));
    assert_eq!(maximum_height(&p, &sources), maximum_height(&p, &[(&south, 2.0)]));
    assert_eq!(tiles.len(), tiles.iter().copied().collect::<HashSet<_>>().len());
    Ok(())
}

#[test]
fn near_far_and_side_planes_cull_only_when_all_eight_corners_are_outside() {
    let mut p = orthographic(5.0, 0.0, 0.0, 64.0, 0.0);
    let center = VisibleTile { z: 6, x: 32, y: 32 };
    p.projection[14] = -0.5;
    p.projection[10] = 0.001;
    assert!(!tile_visible(&p, center, 0.0), "Ground is beyond the near plane");
    assert!(tile_visible(&p, center, 1000.0), "The height slab crosses the near plane");
    p.projection[14] = 2.0;
    assert!(!tile_visible(&p, center, 1000.0), "Entire box is beyond the far plane");
    p.projection[10] = -0.001;
    assert!(tile_visible(&p, center, 2000.0), "Do not reject a box spanning both depth planes");
}

#[test]
fn slab_bounds_keep_camera_inside_slab_and_near_plane_intersections() {
    let mut p = orthographic(3.0, 0.0, 0.0, 100.0, 30.0);
    // Perspective w changes sign inside the slab; four ground/top rays alone
    // cannot enclose the near-plane intersections.
    p.projection[11] = -0.002;
    p.projection[10] = -0.001;
    let height = 1000.0;
    let view = View::new(&p, height);
    let bounds: LatLonBounds = [-180.0, -85.0, 180.0, 85.0].into();
    let actual: HashSet<_> = view.coverage(4, &[bounds], 1024).unwrap().into_iter().collect();
    let expected: HashSet<_> = (0..16)
        .flat_map(|y| (0..16).map(move |x| VisibleTile { z: 4, x, y }))
        .filter(|tile| tile_visible(&p, *tile, height))
        .collect();
    assert!(!expected.is_empty());
    assert!(expected.is_subset(&actual));
}

fn sample_inside_clip(p: &WgpuLayerParameters, tile: VisibleTile, height: f64) -> bool {
    let world = 512.0 * p.zoom.exp2();
    let n = f64::from(1_u32 << tile.z);
    let x = (tile.x as f64 + 0.5) / n * world;
    let y = (f64::from(tile.y) + 0.5) / n * world;
    [0.0, 0.25, 0.499, 0.75, 1.0].into_iter().any(|fraction| {
        let clip: [f64; 4] =
            std::array::from_fn(|r| p.projection[r] * x + p.projection[4 + r] * y + p.projection[8 + r] * height * fraction + p.projection[12 + r]);
        clip[0].abs() <= clip[3] && clip[1].abs() <= clip[3] && clip[2] >= 0.0 && clip[2] <= clip[3]
    })
}

#[test]
fn finite_slab_enclosure_preserves_visible_samples_across_rotated_perspective_views() {
    let bounds: LatLonBounds = [-180.0, -85.0, 180.0, 85.0].into();
    for bearing in [0.0, 37.0, 90.0] {
        for tilt in [-0.002, 0.0, 0.002] {
            let mut p = orthographic(4.0, 0.0, 0.0, 300.0, bearing);
            p.projection[7] = tilt;
            p.projection[11] = -0.002;
            p.projection[15] = 1.0 - tilt * 512.0 * p.zoom.exp2() * 0.5;
            p.projection[10] = -0.001;
            let height = 1000.0;
            let view = View::new(&p, height);
            let tiles: HashSet<_> = view.coverage(5, &[bounds], 1024).unwrap().into_iter().collect();
            let sampled: Vec<_> = (0..32)
                .flat_map(|y| (-32..64).map(move |x| VisibleTile { z: 5, x, y }))
                .filter(|tile| sample_inside_clip(&p, *tile, height))
                .collect();
            assert!(!sampled.is_empty());
            assert!(sampled.iter().all(|tile| tiles.contains(tile)), "bearing={bearing}, tilt={tilt}");
        }
    }
}

#[test]
fn nonfinite_and_pathological_inputs_are_bounded_and_retained_culling_fails_open() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = coverage_source(&directory.path().join("invalid.tif"), [-1.0, -1.0, 1.0, 1.0])?;
    let mut p = parameters(22.0);
    p.projection = [f64::NAN; 16];
    p.width = f64::INFINITY;
    p.height = f64::INFINITY;
    let tiles = visible_tiles(&p, &[(&source, 1.0)], 4);
    assert!(tiles.iter().all(|tile| tile.z <= selected_zoom(&p, &source)));
    assert!(tiles.len() <= 4);
    assert!(tile_visible(&p, VisibleTile { z: 22, x: 1, y: 1 }, f64::NAN));
    assert!(tile_visible(&p, VisibleTile { z: 22, x: 1, y: 1 }, 1.0));
    p.projection = [0.0; 16];
    p.longitude = f64::MAX;
    p.zoom = f64::INFINITY;
    assert!(visible_tiles(&p, &[(&source, 1.0)], 4).len() <= 4);
    Ok(())
}
