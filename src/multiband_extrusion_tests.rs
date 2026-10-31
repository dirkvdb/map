use super::*;
use crate::raster_layer::extrusion_tests::{paint_depth, sample_tile};
use crate::raster_source::NumericTile;
use crate::raster_tiles::test_support::device;

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_multiband_interpolates_values_and_omits_invalid_bars() -> Result<()> {
    let (device, queue) = device()?;
    let mut pipeline = MultibandPipeline::new(&device, (0x16, 0x30, false));
    pipeline.update_palette(&queue, (ColorMapPreset::Gray, ColorMapDirection::Regular));
    let bands = vec![
        Some(NumericTile {
            width: 2,
            height: 2,
            values: vec![40.0, f32::NAN, 20.0, f32::NAN],
        }),
        Some(NumericTile {
            width: 2,
            height: 2,
            values: vec![120.0, f32::NAN, 20.0, f32::NAN],
        }),
    ];
    let texture = tiles::upload_window(&device, &queue, bands, 2)?.expect("nonempty test bands");
    let draw = pipeline.draw_tile(&device, [None; 2], [Some(&texture), Some(&texture)]);
    let data = DrawUniform {
        matrix: [[2.0, 0.0, 0.0, 0.0], [0.0, -2.0, 0.0, 0.0], [0.0, 0.0, -0.2, 0.0], [-1.0, 1.0, 0.6, 1.0]],
        scale: [1.0, 0.0],
        next_scale: [1.0, 0.0],
        range: [0.0, 255.0],
        next_range: [0.0, 255.0],
        parameters: [0.5, 1.0, 64.0, 0.0],
        layers: [0, 1, 1, 0],
        extrusion: [2.0, 2.0, 0.5, 1.0],
        geometry: [0; 4],
    };
    queue.write_buffer(&draw.uniform, 0, bytemuck::bytes_of(&data));
    let image = paint_depth(&device, &queue, |pass| {
        pass.set_pipeline(&pipeline.extrusion_pipeline);
        pass.set_bind_group(0, &draw.bindings, &[]);
        crate::raster_layer::draw_pixels(pass, data.extrusion, data.geometry, &pipeline.extrusion_indices);
    })?;
    assert_eq!(image.get_pixel(16, 16).0, [80, 80, 80, 255]);
    assert_eq!(image.get_pixel(16, 48).0, [20, 20, 20, 255]);
    assert_eq!(image.get_pixel(48, 16).0, [0; 4]);
    assert_eq!(image.get_pixel(48, 48).0, [0; 4]);
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_playback_style_keeps_raw_cache_and_clock() -> Result<()> {
    let (_directory, bands) = tiles::cog_fixture(3)?;
    let (device, queue) = device()?;
    let mut layer = MultibandLayer::new(device, queue)?;
    let base = tests::request(&bands, 0.5, 1).with_style(PlaybackStyle::default().with_extrusion(true, 2.0));
    layer.set_playback(base.clone(), 1.0);
    tests::load_until_idle(&mut layer)?;
    let counts = (layer.requests, layer.uploads);
    let position = layer.playback_frame().expect("accepted frame").position();
    let revision = base.revision();
    let averaged = base
        .clone()
        .with_style(PlaybackStyle::default().with_extrusion(true, 2.0).with_extrusion_averaging(true));
    assert!(base.shares_bands(&averaged));
    assert_eq!(averaged.revision(), revision);
    layer.set_playback(averaged, 1.0);
    // Render with a physical Z scale and depth attachment, like the native host.
    let mut parameters = tests::parameters();
    parameters.depth_format = 0x30;
    parameters.projection[10] = -0.000001;
    parameters.projection[14] = 0.8;
    paint_depth(&layer.device.clone(), &layer.queue.clone(), |pass| layer.render(pass, &parameters))?;
    assert_eq!((layer.requests, layer.uploads), counts);
    let frame = layer.playback_frame().expect("accepted frame");
    assert_eq!((frame.position(), frame.revision()), (position, revision));
    Ok(())
}

fn averaging_uniform() -> DrawUniform {
    DrawUniform {
        scale: [1.0, 0.0],
        next_scale: [1.0, 0.0],
        range: [0.0, 100.0],
        next_range: [0.0, 100.0],
        extrusion: [2.0, 2.0, 0.5, 1.0],
        geometry: [2, 2, 0, 3],
        ..height_uniform()
    }
}

fn height_uniform() -> DrawUniform {
    DrawUniform {
        matrix: crate::raster_layer::extrusion_tests::height_matrix(),
        scale: [2.0, 10.0],
        next_scale: [3.0, 20.0],
        range: [10.0, 30.0],
        next_range: [20.0, 80.0],
        parameters: [0.5, 1.0, 64.0, 0.0],
        layers: [0, 1, 1, 0],
        extrusion: [1.0, 1.0, 0.5, 1.0],
        geometry: [0; 4],
    }
}

fn render_heights(device: &wgpu::Device, queue: &wgpu::Queue, pipeline: &MultibandPipeline, data: DrawUniform, values: [f32; 2]) -> Result<image::RgbaImage> {
    render_bands(device, queue, pipeline, data, values.map(|value| sample_tile([1; 2], &[value])))
}

fn render_bands(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &MultibandPipeline,
    data: DrawUniform,
    bands: [NumericTile; 2],
) -> Result<image::RgbaImage> {
    let size = bands[0].width;
    let bands = bands.into_iter().map(Some).collect();
    let texture = tiles::upload_window(device, queue, bands, size)?.expect("nonempty test bands");
    let draw = pipeline.draw_tile(device, [None; 2], [Some(&texture), Some(&texture)]);
    queue.write_buffer(&draw.uniform, 0, bytemuck::bytes_of(&data));
    paint_depth(device, queue, |pass| {
        pass.set_pipeline(if data.extrusion[3] > 0.0 {
            &pipeline.extrusion_pipeline
        } else {
            &pipeline.pipeline
        });
        pass.set_bind_group(0, &draw.bindings, &[]);
        crate::raster_layer::draw_pixels(pass, data.extrusion, data.geometry, &pipeline.extrusion_indices);
    })
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_multiband_height_normalizes_interpolated_scaled_values_and_ranges() -> Result<()> {
    use crate::raster_layer::extrusion_tests::top_row;
    let (device, queue) = device()?;
    let mut pipeline = MultibandPipeline::new(&device, (0x16, 0x30, false));
    pipeline.update_palette(&queue, (ColorMapPreset::Gray, ColorMapDirection::Regular));
    // The unequal ranges distinguish normalize-after-interpolation from blending
    // normalized endpoint heights. Colors and projected heights must agree.
    for (values, blend, row, color) in [
        ([0.0, 0.0], 0.5, 32, 0),
        ([5.0, 10.0], 0.5, 24, 128),
        ([10.0, 20.0], 0.5, 16, 255),
        ([0.0, 20.0], 0.25, 24, 128),
        ([0.0, 20.0], 0.5, 20, 191),
    ] {
        let mut data = height_uniform();
        data.parameters[0] = blend;
        let image = render_heights(&device, &queue, &pipeline, data, values)?;
        assert_eq!(top_row(&image), Some(row), "values={values:?}, blend={blend}");
        // The preset LUT and UNORM conversion can round a midpoint by one byte.
        assert!(
            image
                .get_pixel(32, row)
                .0
                .into_iter()
                .zip([color, color, color, 255])
                .all(|(actual, expected)| actual.abs_diff(expected) <= 1)
        );
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_multiband_height_respects_masks_clamping_and_zero_maximum() -> Result<()> {
    use crate::raster_layer::extrusion_tests::top_row;
    let (device, queue) = device()?;
    let mut pipeline = MultibandPipeline::new(&device, (0x16, 0x30, false));
    pipeline.update_palette(&queue, (ColorMapPreset::Gray, ColorMapDirection::Regular));
    for (values, row) in [([f32::NAN, 10.0], Some(18)), ([5.0, f32::NAN], Some(30)), ([f32::NAN; 2], None)] {
        let image = render_heights(&device, &queue, &pipeline, height_uniform(), values)?;
        assert_eq!(top_row(&image), row);
    }
    for (value, row) in [(-100.0, 32), (100.0, 16)] {
        let mut data = height_uniform();
        assert_eq!(top_row(&render_heights(&device, &queue, &pipeline, data, [value; 2])?), Some(row));
        data.layers[2] = 0;
        assert_eq!(top_row(&render_heights(&device, &queue, &pipeline, data, [value; 2])?), None);
    }
    let mut data = height_uniform();
    data.extrusion[2] = 0.0;
    let collapsed = render_heights(&device, &queue, &pipeline, data, [5.0, 10.0])?;
    assert_eq!(top_row(&collapsed), Some(32));
    data.extrusion[3] = 0.0;
    assert_eq!(collapsed, render_heights(&device, &queue, &pipeline, data, [5.0, 10.0])?);
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_multiband_endpoints_use_only_the_selected_bands_style_and_validity() -> Result<()> {
    use crate::raster_layer::extrusion_tests::top_row;
    let (device, queue) = device()?;
    let mut pipeline = MultibandPipeline::new(&device, (0x16, 0x30, false));
    pipeline.update_palette(&queue, (ColorMapPreset::Gray, ColorMapDirection::Regular));
    for (blend, values, row, color) in [(0.0, [5.0, f32::NAN], 24, 64), (1.0, [f32::NAN, 15.0], 20, 96)] {
        let mut data = height_uniform();
        data.parameters = [blend, 0.5, 64.0, 0.0];
        let image = render_heights(&device, &queue, &pipeline, data, values)?;
        assert_eq!(top_row(&image), Some(row), "blend={blend}");
        let actual = image.get_pixel(32, row).0;
        assert!(
            actual.into_iter().zip([color, color, color, 128]).all(|(a, b)| a.abs_diff(b) <= 1),
            "blend={blend}, color={actual:?}"
        );
        // A valid unused band must not resurrect an invalid selected endpoint.
        let invalid = render_heights(&device, &queue, &pipeline, data, [values[1], values[0]])?;
        assert!(invalid.pixels().all(|pixel| pixel.0 == [0; 4]), "blend={blend}");
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_multiband_lighting_shades_compass_walls_and_preserves_premultiplied_alpha() -> Result<()> {
    use crate::raster_layer::extrusion_tests::{assert_bar_faces, rotated_height_matrix, write_solid_palette};
    let (device, queue) = device()?;
    let pipeline = MultibandPipeline::new(&device, (0x16, 0x30, false));
    write_solid_palette(&queue, &pipeline.palette, [192, 128, 64, 128]);
    for (azimuth, wall) in ["south", "east", "north", "west"].into_iter().enumerate() {
        let mut data = height_uniform();
        data.matrix = rotated_height_matrix(azimuth);
        data.parameters = [1.0, 0.5, 64.0, 0.0];
        let image = render_heights(&device, &queue, &pipeline, data, [f32::NAN, 20.0])?;
        // Premultiplied BGRA, matching the native-host readback contract.
        assert_bar_faces(&image, [16, 32, 48, 64], wall);
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_multiband_lod_preserves_interpolated_heights_and_edge_cells() -> Result<()> {
    use crate::raster_layer::extrusion_tests::top_row;
    let (device, queue) = device()?;
    let mut pipeline = MultibandPipeline::new(&device, (0x16, 0x30, false));
    pipeline.update_palette(&queue, (ColorMapPreset::Gray, ColorMapDirection::Regular));
    let mut data = height_uniform();
    data.extrusion[0] = 5.0;
    data.extrusion[1] = 3.0;
    let reference = render_heights(&device, &queue, &pipeline, data, [5.0, 10.0])?;
    assert_eq!(top_row(&reference), Some(24));
    for flags in 0..8 {
        data.geometry = [2, 2, flags, 0];
        let image = render_heights(&device, &queue, &pipeline, data, [5.0, 10.0])?;
        assert_eq!(top_row(&image), Some(24), "LOD must preserve interpolate-then-normalize height");
        for (x, y) in [(17, 28), (46, 28), (17, 46), (46, 46)] {
            assert_eq!(image.get_pixel(x, y), reference.get_pixel(x, y), "Partial edge at ({x}, {y}), flags={flags}");
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_multiband_retained_ancestors_do_not_cap_request_detail() -> Result<()> {
    let (_directory, bands) = tiles::cog_fixture(2)?;
    let (device, queue) = device()?;
    let mut layer = MultibandLayer::new(device, queue)?;
    let request = tests::request(&bands, 0.0, 1).with_style(PlaybackStyle::default().with_extrusion(true, 1.0));
    layer.set_playback(request.clone(), 1.0);
    let p = crate::raster_tiles::test_support::parameters(5.0);
    let target = layer.view_tiles(&p, &request, &bands[0]);
    assert!(!target.is_empty());
    assert!(target.iter().all(|tile| tile.z > 0));
    let root = WindowKey {
        tile: geo::Tile { z: 0, x: 0, y: 0 },
        start: 0,
    };
    layer.cache.insert(root, CachedWindow { texture: None, used: 0 });
    assert_eq!(layer.view_tiles(&p, &request, &bands[0]), target);
    let pipeline = MultibandPipeline::new(&layer.device, (0x16, 0, false));
    let draw = pipeline.draw_tile(&layer.device, [None; 2], [None; 2]);
    layer.retained = Some(RetainedFrame {
        draws: std::collections::HashMap::from([(VisibleTile { z: 0, x: 0, y: 0 }, draw)]),
        frame: endpoints(0.0, bands.len(), true),
        window_size: layer.window_size,
    });
    assert_eq!(
        layer.view_tiles(&p, &request, &bands[0]),
        target,
        "A retained coarse grid must not cap its own replacement"
    );
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_multiband_zero_height_matches_flat_with_masked_endpoints_and_blends() -> Result<()> {
    let (device, queue) = device()?;
    let mut pipeline = MultibandPipeline::new(&device, (0x16, 0x30, false));
    pipeline.update_palette(&queue, (ColorMapPreset::Gray, ColorMapDirection::Regular));
    for (values, alphas) in [
        ([5.0, 10.0], [255, 255, 255]),
        ([f32::NAN, 10.0], [0, 128, 255]),
        ([5.0, f32::NAN], [255, 128, 0]),
        ([f32::NAN; 2], [0, 0, 0]),
    ] {
        for (blend, alpha) in [0.0, 0.5, 1.0].into_iter().zip(alphas) {
            let mut data = height_uniform();
            data.parameters[0] = blend;
            data.extrusion[2] = 0.0;
            let collapsed = render_heights(&device, &queue, &pipeline, data, values)?;
            data.extrusion[3] = 0.0;
            let flat = render_heights(&device, &queue, &pipeline, data, values)?;
            assert_eq!(collapsed, flat, "blend={blend}, values={values:?}");
            assert_eq!(flat.get_pixel(32, 40)[3], alpha, "blend={blend}, values={values:?}");
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_multiband_averaging_uses_raw_mean_before_endpoint_clamping() -> Result<()> {
    use crate::raster_layer::extrusion_tests::top_row;
    let (device, queue) = device()?;
    let mut pipeline = MultibandPipeline::new(&device, (0x16, 0x30, false));
    pipeline.update_palette(&queue, (ColorMapPreset::Gray, ColorMapDirection::Regular));
    for (values, mean, row) in [([-100.0, 0.0, 0.0, 100.0], 0.0, 32), ([0.0, 0.0, 0.0, 100.0], 25.0, 28)] {
        for (blend, clamp) in [(0.0, 0), (0.0, 1), (1.0, 0), (1.0, 1)] {
            let mut data = averaging_uniform();
            data.parameters[0] = blend;
            data.layers[2] = clamp;
            let image = render_bands(&device, &queue, &pipeline, data, std::array::from_fn(|_| sample_tile([2; 2], &values)))?;
            assert_eq!(top_row(&image), Some(row), "values={values:?}, blend={blend}, clamp={clamp}");
            assert_eq!(image.get_pixel(32, row)[3], 255);
            data.geometry[3] = 0;
            assert_eq!(image, render_heights(&device, &queue, &pipeline, data, [mean; 2])?);
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_multiband_averaging_styles_unequal_endpoint_counts_before_value_interpolation() -> Result<()> {
    use crate::raster_layer::extrusion_tests::top_row;
    let (device, queue) = device()?;
    let mut pipeline = MultibandPipeline::new(&device, (0x16, 0x30, false));
    // A nonlinear palette distinguishes one lookup after value interpolation from blended colors.
    pipeline.update_palette(&queue, (ColorMapPreset::Blues, ColorMapDirection::Regular));
    for (current, next, means, row) in [
        ([0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY], [10.0, 20.0, 30.0, f32::NAN], [0.0, 20.0], 20),
        ([0.0, 10.0, f32::NAN, f32::INFINITY], [0.0, 10.0, 20.0, f32::NAN], [5.0, 10.0], 24),
        ([0.0, 5.0, 10.0, f32::NAN], [f32::NAN, 10.0, f32::NAN, f32::INFINITY], [5.0, 10.0], 24),
    ] {
        let mut data = DrawUniform {
            extrusion: [2.0, 2.0, 0.5, 1.0],
            geometry: [2, 2, 0, 3],
            ..height_uniform()
        };
        let bands = [sample_tile([2; 2], &current), sample_tile([2; 2], &next)];
        let image = render_bands(&device, &queue, &pipeline, data, bands)?;
        assert_eq!(top_row(&image), Some(row), "means={means:?}");
        assert_eq!(image.get_pixel(32, row)[3], 255, "Partial contributor counts still form fully valid endpoints");
        data.geometry[3] = 0;
        assert_eq!(image, render_heights(&device, &queue, &pipeline, data, means)?, "means={means:?}");
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_multiband_averaging_flags_select_current_and_next_means_independently() -> Result<()> {
    use crate::raster_layer::extrusion_tests::top_row;
    let (device, queue) = device()?;
    let mut pipeline = MultibandPipeline::new(&device, (0x16, 0x30, false));
    pipeline.update_palette(&queue, (ColorMapPreset::Gray, ColorMapDirection::Regular));
    for (flags, samples, rows) in [
        (0, [10.0, 20.0], [16, 16, 16]),
        (1, [2.5, 20.0], [28, 19, 16]),
        (2, [10.0, 5.0], [16, 25, 28]),
        (3, [2.5, 5.0], [28, 28, 28]),
    ] {
        for (blend, row) in [0.0, 0.5, 1.0].into_iter().zip(rows) {
            let mut data = DrawUniform {
                extrusion: [2.0, 2.0, 0.5, 1.0],
                geometry: [2, 2, 0, flags],
                ..height_uniform()
            };
            data.parameters[0] = blend;
            let bands = [sample_tile([2; 2], &[0.0, 0.0, 0.0, 10.0]), sample_tile([2; 2], &[0.0, 0.0, 0.0, 20.0])];
            let image = render_bands(&device, &queue, &pipeline, data, bands)?;
            assert_eq!(top_row(&image), Some(row), "flags={flags}, blend={blend}");
            data.geometry[3] = 0;
            assert_eq!(
                image,
                render_heights(&device, &queue, &pipeline, data, samples)?,
                "flags={flags}, blend={blend}"
            );
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_multiband_averaging_fades_only_missing_endpoints_and_omits_all_nodata() -> Result<()> {
    let (device, queue) = device()?;
    let mut pipeline = MultibandPipeline::new(&device, (0x16, 0x30, false));
    pipeline.update_palette(&queue, (ColorMapPreset::Gray, ColorMapDirection::Regular));
    let partial = [50.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY];
    let missing = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::NAN];
    for (values, means, alphas) in [
        ([partial, missing], [50.0, f32::NAN], [255, 191, 0]),
        ([missing, partial], [f32::NAN, 50.0], [0, 64, 255]),
        ([missing; 2], [f32::NAN; 2], [0, 0, 0]),
    ] {
        for (blend, alpha) in [0.0, 0.25, 1.0].into_iter().zip(alphas) {
            let mut data = averaging_uniform();
            data.parameters[0] = blend;
            let image = render_bands(&device, &queue, &pipeline, data, values.map(|band| sample_tile([2; 2], &band)))?;
            assert_eq!(image.get_pixel(32, 26)[3], alpha, "means={means:?}, blend={blend}");
            data.geometry[3] = 0;
            assert_eq!(
                image,
                render_heights(&device, &queue, &pipeline, data, means)?,
                "means={means:?}, blend={blend}"
            );
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_multiband_averaging_covers_odd_logical_grid_edges_once() -> Result<()> {
    use crate::raster_layer::extrusion_tests::odd_averaging_values;
    let (device, queue) = device()?;
    let mut pipeline = MultibandPipeline::new(&device, (0x16, 0x30, false));
    pipeline.update_palette(&queue, (ColorMapPreset::Gray, ColorMapDirection::Regular));
    let (values, means) = odd_averaging_values();
    // Window uploads are square; the 5x3 logical grid samples rows 0, 2 and 4.
    let square = |logical: [f32; 15]| {
        let mut values = [f32::NAN; 25];
        for row in 0..3 {
            values[row * 10..row * 10 + 5].copy_from_slice(&logical[row * 5..row * 5 + 5]);
        }
        sample_tile([5; 2], &values)
    };
    let mut data = DrawUniform {
        matrix: [[2.0, 0.0, 0.0, 0.0], [0.0, -2.0, 0.0, 0.0], [0.0, 0.0, -0.2, 0.0], [-1.0, 1.0, 0.6, 1.0]],
        range: [0.0, 200.0],
        next_range: [0.0, 200.0],
        parameters: [0.5, 0.5, 64.0, 0.0],
        extrusion: [5.0, 3.0, 0.5, 1.0],
        geometry: [1, 1, 0, 0],
        ..averaging_uniform()
    };
    let reference = render_bands(&device, &queue, &pipeline, data, std::array::from_fn(|_| square(means)))?;
    assert!(reference.pixels().all(|pixel| pixel[3] == 128));
    for order in 0..8 {
        data.geometry = [2, 2, order, 3];
        let image = render_bands(&device, &queue, &pipeline, data, std::array::from_fn(|_| square(values)))?;
        assert_eq!(image, reference, "Odd edge means, coverage and alpha must match: order={order}");
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_multiband_averaging_flags_leave_flat_sampling_unchanged() -> Result<()> {
    let (device, queue) = device()?;
    let mut pipeline = MultibandPipeline::new(&device, (0x16, 0x30, false));
    pipeline.update_palette(&queue, (ColorMapPreset::Gray, ColorMapDirection::Regular));
    let mut data = averaging_uniform();
    data.parameters[0] = 0.25;
    data.extrusion[3] = 0.0;
    data.geometry[3] = 0;
    let bands = || [sample_tile([2; 2], &[0.0, 25.0, 75.0, 100.0]), sample_tile([2; 2], &[100.0, 100.0, 25.0, 0.0])];
    let reference = render_bands(&device, &queue, &pipeline, data, bands())?;
    assert_ne!(
        reference.get_pixel(24, 36),
        reference.get_pixel(40, 36),
        "Flat sampling must retain pixel detail"
    );
    for flags in 1..=3 {
        data.geometry[3] = flags;
        assert_eq!(render_bands(&device, &queue, &pipeline, data, bands())?, reference, "flags={flags}");
    }
    Ok(())
}
