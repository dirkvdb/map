use super::*;

const VIEWPORT: [f64; 2] = [1024.0, 1024.0];
const EXTRUSION: [f32; 4] = [1024.0, 1024.0, 1.0, 1.0];

fn matrix(scale: [f32; 2]) -> [[f32; 4]; 4] {
    [[scale[0], 0.0, 0.0, 0.0], [0.0, scale[1], 0.0, 0.0], [0.0, 0.0, 0.0, 0.0], [0.0, 0.0, 0.5, 1.0]]
}

fn parameters() -> WgpuLayerParameters {
    WgpuLayerParameters {
        width: VIEWPORT[0],
        height: VIEWPORT[1],
        latitude: 0.0,
        longitude: 0.0,
        zoom: 0.0,
        projection: [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0],
        color_format: 0,
        depth_format: 0,
        depth_test: true,
    }
}

fn tile(x: i64, y: i32) -> VisibleTile {
    VisibleTile { z: 2, x, y }
}

#[test]
fn extrusion_averaging_combines_four_cells_without_changing_traversal_or_lod() {
    let near = [1, 1, 7, 0];
    let far = [8, 4, 5, 0];
    for blend in [0.0, 0.5, 1.0] {
        assert_eq!(with_averaging(near, 3, blend), [2, 2, 7, 3]);
        assert_eq!(with_averaging(far, 3, blend), [8, 4, 5, 3]);
        assert_eq!(with_averaging(near, 0, blend), near);
    }
    assert_eq!(instance_count([256.0, 256.0, 1.0, 1.0], with_averaging(near, 3, 1.0)), 16_384);
    assert_eq!(instance_count([5.0, 3.0, 1.0, 1.0], with_averaging(near, 3, 1.0)), 6);
}

#[test]
fn extrusion_averaging_keeps_fine_geometry_during_mixed_mode_fades() {
    for (flags, first, last) in [(1, 2, 1), (2, 1, 2)] {
        let first_plan = with_averaging([1, 1, 0, 0], flags, 0.0);
        let last_plan = with_averaging([1, 1, 0, 0], flags, 1.0);
        assert_eq!(&first_plan[..2], &[first, first]);
        assert_eq!(&last_plan[..2], &[last, last]);
        for blend in [0.25, 0.5, 0.75] {
            assert_eq!(with_averaging([1, 1, 0, 0], flags, blend), [1, 1, 0, flags]);
        }
    }
}

#[test]
fn angle_reversals_and_transposition_produce_all_eight_flags() {
    let gradients = [
        [0.0625, 0.25],
        [-0.0625, 0.25],
        [0.0625, -0.25],
        [-0.0625, -0.25],
        [0.25, 0.0625],
        [-0.25, 0.0625],
        [0.25, -0.0625],
        [-0.25, -0.0625],
    ];
    let flags = gradients.map(|[dx, dy]| {
        let mut projection = matrix([1.0, 1.0]);
        projection[0][2] = dx;
        projection[1][2] = dy;
        plan(&projection, VIEWPORT, EXTRUSION, true)[2]
    });
    assert_eq!(flags, [0, 1, 2, 3, 4, 5, 6, 7]);
}

#[test]
fn perspective_order_uses_depth_quotient_not_clip_z_alone() {
    let mut projection = matrix([1.0, 1.0]);
    projection[0][2] = 0.125;
    projection[0][3] = 1.0;
    assert_eq!(plan(&projection, VIEWPORT, EXTRUSION, true)[2], 5);
}

#[test]
fn transparent_draws_keep_original_order_even_when_coarsened() {
    let mut projection = matrix([0.015625, 0.015625]);
    projection[0][2] = -0.25;
    projection[1][2] = -0.0625;
    assert_eq!(plan(&projection, VIEWPORT, EXTRUSION, false), [16, 16, 0, 0]);
}

#[test]
fn zero_depth_gradient_keeps_original_order() {
    assert_eq!(plan(&matrix([1.0, 1.0]), VIEWPORT, EXTRUSION, true)[2], 0);
}

#[test]
fn equal_depth_gradients_use_row_major_tie_break() {
    let mut projection = matrix([1.0, 1.0]);
    projection[0][2] = -0.125;
    projection[1][2] = -0.125;
    assert_eq!(plan(&projection, VIEWPORT, EXTRUSION, true)[2], 3);
}

#[test]
fn disabled_flat_draws_keep_full_detail() {
    assert_eq!(plan(&matrix([0.001, 0.001]), VIEWPORT, [1024.0, 1024.0, 1.0, 0.0], true), [1, 1, 0, 0]);
}

#[test]
fn resolvable_cells_keep_full_detail() {
    assert_eq!(plan(&matrix([2.0, 2.0]), VIEWPORT, EXTRUSION, true), [1, 1, 0, 0]);
}

#[test]
fn cells_between_half_and_one_pixel_are_not_coarsened() {
    assert_eq!(plan(&matrix([1.5, 1.5]), VIEWPORT, EXTRUSION, true), [1, 1, 0, 0]);
}

#[test]
fn far_nonsquare_grid_reduces_instance_count() {
    let extrusion = [512.0, 256.0, 1.0, 1.0];
    let geometry = plan(&matrix([0.125, 0.125]), [512.0, 512.0], extrusion, true);
    assert_eq!((geometry, instance_count(extrusion, geometry)), ([8, 4, 0, 0], 4096));
}

#[test]
fn anisotropic_projection_only_coarsens_the_subpixel_axis() {
    let extrusion = [256.0, 256.0, 1.0, 1.0];
    assert_eq!(plan(&matrix([2.0, 0.015625]), [512.0, 512.0], extrusion, true), [1, 16, 0, 0]);
}

#[test]
fn viewport_aspect_ratio_is_applied_per_screen_axis() {
    assert_eq!(plan(&matrix([1.0, 1.0]), [1024.0, 64.0], EXTRUSION, true), [1, 16, 0, 0]);
}

#[test]
fn projected_axis_length_uses_both_screen_components() {
    let mut projection = matrix([0.0, 1.0 / 1024.0]);
    projection[0][0] = 3.0 / 1024.0;
    projection[0][1] = 4.0 / 1024.0;
    assert_eq!(plan(&projection, VIEWPORT, [16.0, 16.0, 1.0, 1.0], true), [2, 16, 0, 0]);
}

#[test]
fn strides_never_exceed_their_grid_axis() {
    assert_eq!(plan(&matrix([0.0001, 0.0001]), VIEWPORT, [3.0, 7.0, 1.0, 1.0], true), [2, 4, 0, 0]);
}

#[test]
fn single_cell_axes_cannot_be_coarsened() {
    assert_eq!(plan(&matrix([0.0, 0.0]), VIEWPORT, [1.0, 1.0, 1.0, 1.0], true), [1, 1, 0, 0]);
}

#[test]
fn exact_half_pixel_coarsened_footprint_is_allowed() {
    assert_eq!(plan(&matrix([1.0, 1.0]), VIEWPORT, [2048.0, 2048.0, 1.0, 1.0], true), [2, 2, 0, 0]);
}

#[test]
fn near_plane_crossing_retains_full_detail() {
    let mut projection = matrix([0.001, 0.001]);
    projection[0][2] = -1.0;
    assert_eq!(plan(&projection, VIEWPORT, EXTRUSION, true), [1, 1, 0, 0]);
}

#[test]
fn near_plane_crossing_at_only_the_upper_far_corner_retains_full_detail() {
    let mut projection = matrix([0.001, 0.001]);
    projection[0][2] = -1.0;
    projection[1][2] = -1.0;
    projection[2][2] = -1.0;
    projection[3][2] = 2.5;
    assert_eq!(plan(&projection, VIEWPORT, EXTRUSION, true), [1, 1, 0, 0]);
}

#[test]
fn eye_plane_crossing_at_only_the_upper_far_corner_retains_full_detail() {
    let mut projection = matrix([0.001, 0.001]);
    projection[0][3] = -1.0;
    projection[1][3] = -1.0;
    projection[2][3] = -1.0;
    projection[3][3] = 3.0;
    assert_eq!(plan(&projection, VIEWPORT, EXTRUSION, true), [1, 1, 0, 0]);
}

#[test]
fn nonpositive_and_near_zero_w_retain_full_detail() {
    for w in [-1.0, 0.0, 0.0000005] {
        let mut projection = matrix([0.001, 0.001]);
        projection[3][3] = w;
        assert_eq!(plan(&projection, VIEWPORT, EXTRUSION, true), [1, 1, 0, 0]);
    }
}

#[test]
fn maximum_physical_height_limits_lod() {
    let mut projection = matrix([0.125, 0.125]);
    projection[3][3] = 8.0;
    projection[2][3] = -0.5;
    let ground = [1024.0, 1024.0, 0.0, 1.0];
    let tall = [1024.0, 1024.0, 14.0, 1.0];
    let ground_plan = plan(&projection, VIEWPORT, ground, true);
    let tall_plan = plan(&projection, VIEWPORT, tall, true);
    assert_eq!(([ground_plan[0], ground_plan[1]], [tall_plan[0], tall_plan[1]]), ([16, 16], [1, 1]));
}

#[test]
fn full_tile_derivative_bound_is_not_a_center_or_endpoint_estimate() {
    let mut projection = matrix([0.125, 0.125]);
    projection[0][3] = 8.0;
    projection[3][3] = 0.5;
    // Changing X also moves screen Y through W; that contribution peaks off-center.
    assert_eq!(&plan(&projection, VIEWPORT, EXTRUSION, true)[..2], &[1, 1]);
}

#[test]
fn nonfinite_matrix_components_retain_full_detail() {
    for index in 0..16 {
        let mut projection = matrix([0.001, 0.001]);
        projection[index / 4][index % 4] = f32::NAN;
        assert_eq!(plan(&projection, VIEWPORT, EXTRUSION, true), [1, 1, 0, 0]);
    }
}

#[test]
fn invalid_viewports_retain_full_detail() {
    for viewport in [[0.0, 1024.0], [1024.0, -1.0], [f64::NAN, 1024.0], [1024.0, f64::INFINITY]] {
        assert_eq!(plan(&matrix([0.001, 0.001]), viewport, EXTRUSION, true), [1, 1, 0, 0]);
    }
}

#[test]
fn invalid_extrusion_inputs_retain_full_detail() {
    for extrusion in [[0.0, 1024.0, 1.0, 1.0], [1024.0, f32::NAN, 1.0, 1.0], [1024.0, 1024.0, -1.0, 1.0]] {
        assert_eq!(plan(&matrix([0.001, 0.001]), VIEWPORT, extrusion, true), [1, 1, 0, 0]);
    }
}

#[test]
fn odd_grid_keeps_partial_edge_cells() {
    assert_eq!(instance_count([17.0, 9.0, 1.0, 1.0], [16, 4, 0, 0]), 6);
}

#[test]
fn missing_geometry_zeros_default_to_unit_strides() {
    assert_eq!(instance_count([17.0, 9.0, 1.0, 1.0], [0; 4]), 153);
}

#[test]
fn each_zero_stride_defaults_independently() {
    assert_eq!(instance_count([17.0, 9.0, 1.0, 1.0], [0, 4, 7, 0]), 51);
}

#[test]
fn unrepresentable_instance_counts_saturate_instead_of_wrapping() {
    assert_eq!(instance_count([65536.0, 65536.0, 1.0, 1.0], [0; 4]), u32::MAX);
}

#[test]
fn wrapped_tile_sorting_uses_unwrapped_ground_centers() {
    let mut parameters = parameters();
    parameters.projection[2] = -1.0 / 512.0;
    parameters.projection[14] = 4.0;
    let mut tiles = [tile(-1, 0), tile(3, 0), tile(7, 0)];
    sort_tiles(&parameters, &mut tiles);
    assert_eq!(tiles, [tile(7, 0), tile(3, 0), tile(-1, 0)]);
}

#[test]
fn tile_sorting_uses_depth_quotient_not_clip_z_alone() {
    let mut parameters = parameters();
    parameters.projection[2] = 1.0 / 512.0;
    parameters.projection[3] = 1.0 / 512.0;
    parameters.projection[14] = 2.0;
    let mut tiles = [tile(0, 0), tile(3, 0), tile(1, 0)];
    sort_tiles(&parameters, &mut tiles);
    assert_eq!(tiles, [tile(3, 0), tile(1, 0), tile(0, 0)]);
}

#[test]
fn equal_tile_depths_have_deterministic_coordinate_ties() {
    let parameters = parameters();
    let mut tiles = [tile(2, 1), tile(1, 0), tile(0, 1), tile(0, 0)];
    sort_tiles(&parameters, &mut tiles);
    assert_eq!(tiles, [tile(0, 0), tile(1, 0), tile(0, 1), tile(2, 1)]);
}

#[test]
fn invalid_projection_preserves_input_tile_order() {
    let mut parameters = parameters();
    parameters.projection[14] = f64::NAN;
    let mut tiles = [tile(3, 1), tile(-1, 0), tile(0, 0)];
    let original = tiles;
    sort_tiles(&parameters, &mut tiles);
    assert_eq!(tiles, original);
}

#[test]
fn invalid_depths_follow_valid_tiles_and_keep_their_relative_order() {
    let mut parameters = parameters();
    parameters.projection[3] = 1.0 / 128.0;
    parameters.projection[15] = 0.0;
    parameters.projection[14] = 1.0;
    let mut tiles = [tile(-1, 0), tile(0, 0), tile(-2, 0), tile(1, 0)];
    sort_tiles(&parameters, &mut tiles);
    assert_eq!(tiles, [tile(1, 0), tile(0, 0), tile(-1, 0), tile(-2, 0)]);
}

#[test]
fn tile_depth_retains_f64_precision_at_high_zoom() {
    let mut parameters = parameters();
    parameters.zoom = 22.0;
    parameters.projection[2] = 1.0;
    parameters.projection[14] = -1_024_000_000.0;
    let mut tiles = [VisibleTile { z: 22, x: 2_000_001, y: 0 }, VisibleTile { z: 22, x: 2_000_000, y: 0 }];
    sort_tiles(&parameters, &mut tiles);
    assert_eq!(tiles[0].x, 2_000_000);
}

#[test]
fn tile_depth_accounts_for_tile_zoom() {
    let mut parameters = parameters();
    parameters.projection[2] = 1.0;
    let mut tiles = [VisibleTile { z: 1, x: 1, y: 0 }, VisibleTile { z: 2, x: 1, y: 0 }];
    sort_tiles(&parameters, &mut tiles);
    assert_eq!(tiles[0].z, 2);
}
