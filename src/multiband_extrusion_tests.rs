use super::*;
use crate::raster_layer::extrusion_tests::paint_depth;
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
    };
    queue.write_buffer(&draw.uniform, 0, bytemuck::bytes_of(&data));
    let image = paint_depth(&device, &queue, |pass| {
        pass.set_pipeline(&pipeline.extrusion_pipeline);
        pass.set_bind_group(0, &draw.bindings, &[]);
        crate::raster_layer::draw_pixels(pass, data.extrusion);
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
    let base = tests::request(&bands, 0.5, 1);
    layer.set_playback(base.clone(), 1.0);
    tests::load_until_idle(&mut layer)?;
    let counts = (layer.requests, layer.uploads);
    let position = layer.playback_frame().expect("accepted frame").position();
    layer.set_playback(base.with_style(PlaybackStyle::default().with_extrusion(true, 2.0)), 1.0);
    // Render with a physical Z scale and depth attachment, like the native host.
    let mut parameters = tests::parameters();
    parameters.depth_format = 0x30;
    parameters.projection[10] = -0.000001;
    parameters.projection[14] = 0.8;
    paint_depth(&layer.device.clone(), &layer.queue.clone(), |pass| layer.render(pass, &parameters))?;
    assert_eq!((layer.requests, layer.uploads), counts);
    assert_eq!(layer.playback_frame().expect("accepted frame").position(), position);
    Ok(())
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
    }
}

fn render_heights(device: &wgpu::Device, queue: &wgpu::Queue, pipeline: &MultibandPipeline, data: DrawUniform, values: [f32; 2]) -> Result<image::RgbaImage> {
    let bands = values
        .into_iter()
        .map(|value| {
            Some(NumericTile {
                width: 1,
                height: 1,
                values: vec![value],
            })
        })
        .collect();
    let texture = tiles::upload_window(device, queue, bands, 1)?.expect("nonempty test bands");
    let draw = pipeline.draw_tile(device, [None; 2], [Some(&texture), Some(&texture)]);
    queue.write_buffer(&draw.uniform, 0, bytemuck::bytes_of(&data));
    paint_depth(device, queue, |pass| {
        pass.set_pipeline(if data.extrusion[3] > 0.0 {
            &pipeline.extrusion_pipeline
        } else {
            &pipeline.pipeline
        });
        pass.set_bind_group(0, &draw.bindings, &[]);
        crate::raster_layer::draw_pixels(pass, data.extrusion);
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
