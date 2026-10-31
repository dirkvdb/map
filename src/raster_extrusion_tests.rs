use super::*;
use crate::raster_playback::{PlaybackRequest, PlaybackStyle};
use crate::raster_tiles::test_support::{device, parameters};

#[test]
fn extrusion_snapshots_preserve_style_and_raw_identity() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("extrusion.tif");
    crate::raster_source::test_support::write_byte_raster(&path)?;
    let source = RasterSource::open(&path)?.remove(0);
    assert_eq!((source.is_extruded(), source.extrusion_height()), (false, 1.0));
    let raised = source.with_extrusion(true, 3.0)?;
    let changed = raised
        .restyled(ColorMapPreset::Gray, (0.0, 255.0))?
        .with_clamp_colors(false)?
        .with_color_map_direction(ColorMapDirection::Reversed)?
        .with_unscaling(false)?;
    assert_eq!((changed.is_extruded(), changed.extrusion_height()), (true, 3.0));
    assert_eq!(changed.metadata().id, source.metadata().id);
    assert_ne!(changed.id(), source.id());
    let request = PlaybackRequest::new(vec![changed.clone()], 0.0, 1, false, 2.0, true);
    assert_eq!(request.style().extrusion_for(&changed), (true, 3.0));
    let style = PlaybackStyle::default().with_extrusion(false, 8.0);
    let updated = request.clone().with_style(style).with_controls(0.0, 2, true, 3.0).resumed_from(None);
    assert!(request.shares_bands(&updated));
    assert_eq!(updated.style().extrusion(), Some((false, 8.0)));
    assert_eq!(updated.style().extrusion_for(&changed), (false, 8.0));
    Ok(())
}

#[test]
fn extrusion_height_normalizes_and_parent_grid_preserves_pixel_width() {
    for (input, expected) in [(-1.0, 0.0), (11.0, 10.0), (f32::NAN, 1.0), (f32::INFINITY, 1.0)] {
        assert_eq!(PlaybackStyle::default().with_extrusion(true, input).extrusion(), Some((true, expected)));
    }
    let full = [0.0, 0.0, 1.0, 1.0];
    let quarter = [0.5, 0.0, 0.5, 0.5];
    assert_eq!(extrusion_grid(Some([256; 2]), quarter, None, full, 1.0), [128; 2]);
    assert_eq!(extrusion_grid(Some([256; 2]), full, Some([256; 2]), quarter, 0.0), [128; 2]);
    let parent = extrusion_uniform(&parameters(2.0), VisibleTile { z: 2, x: 0, y: 0 }, [256; 2], 3.0);
    let child = extrusion_uniform(&parameters(2.0), VisibleTile { z: 3, x: 0, y: 0 }, [128; 2], 3.0);
    assert_eq!(parent[2], child[2]);
    assert!((parent[2] - (std::f64::consts::TAU * 6_378_137.0 / 4.0 / 256.0 * 30.0) as f32).abs() < 0.01);
}

pub(crate) fn paint_depth(device: &wgpu::Device, queue: &wgpu::Queue, render: impl FnOnce(&mut wgpu::RenderPass<'_>)) -> Result<image::RgbaImage> {
    let texture = |format| {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Extrusion regression target"),
            size: wgpu::Extent3d {
                width: 64,
                height: 64,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
    };
    let color = texture(wgpu::TextureFormat::Rgba8Unorm);
    let depth = texture(wgpu::TextureFormat::Depth32Float);
    let color_view = color.create_view(&Default::default());
    let depth_view = depth.create_view(&Default::default());
    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        render(&mut pass);
    }
    queue.submit([encoder.finish()]);
    crate::map_readback::GpuReadback::new(device).read(&color, device, queue)
}

fn uniform() -> DrawUniform {
    DrawUniform {
        matrix: [[2.0, 0.0, 0.0, 0.0], [0.0, -2.0, 0.0, 0.0], [0.0, 0.0, -0.2, 0.0], [-1.0, 1.0, 0.6, 1.0]],
        uv: [0.0, 0.0, 1.0, 1.0],
        previous_uv: [0.0, 0.0, 1.0, 1.0],
        range: [0.0, 255.0],
        previous_range: [0.0, 255.0],
        scale_offset: [1.0, 0.0],
        previous_scale_offset: [1.0, 0.0],
        opacity: 1.0,
        blend: 1.0,
        tile_size: 64.0,
        previous_extrusion_height: 0.5,
        tile_id: [0; 4],
        clamp_colors: [0; 4],
        extrusion: [2.0, 2.0, 0.5, 1.0],
    }
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_omits_nodata_out_of_range_and_transparent_palette_without_occluding() -> Result<()> {
    let (device, queue) = device()?;
    let pipeline = RasterPipeline::new(&device, (0x16, 0x30, false));
    let palette: Vec<u8> = (0..=255).flat_map(|v| [v, v, v, if v == 200 { 0 } else { 255 }]).collect();
    write_texture(&queue, &pipeline.palettes[0], &palette, 256, 1, 4);
    let front = upload_tile(
        &device,
        &queue,
        NumericTile {
            width: 2,
            height: 2,
            values: vec![80.0, f32::NAN, 300.0, 200.0],
        },
    );
    let back = upload_tile(
        &device,
        &queue,
        NumericTile {
            width: 2,
            height: 2,
            values: vec![160.0; 4],
        },
    );
    let draws = [front, back].each_ref().map(|t| pipeline.draw_tile(&device, (None, None), Some(t), None));
    for (index, draw) in draws.iter().enumerate() {
        let mut data = uniform();
        data.matrix[3][2] += index as f32 * 0.2;
        queue.write_buffer(&draw.uniform, 0, bytemuck::bytes_of(&data));
    }
    let render = |order: [usize; 2]| {
        paint_depth(&device, &queue, |pass| {
            pass.set_pipeline(&pipeline.extrusion_pipeline);
            for index in order {
                pass.set_bind_group(0, &draws[index].bindings, &[]);
                draw_pixels(pass, uniform().extrusion);
            }
        })
    };
    let image = render([0, 1])?;
    assert_eq!(image, render([1, 0])?, "Depth must resolve independently of tile submission order");
    for (x, y, value) in [(16, 16, 80), (48, 16, 160), (16, 48, 160), (48, 48, 160)] {
        assert_eq!(image.get_pixel(x, y).0, [value, value, value, 255]);
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_parent_uv_and_outgoing_snapshot_keep_pixel_color_on_walls() -> Result<()> {
    let (device, queue) = device()?;
    let pipeline = RasterPipeline::new(&device, (0x16, 0x30, false));
    let palette: Vec<u8> = (0..=255).flat_map(|v| [v, v, v, 255]).collect();
    for texture in &pipeline.palettes {
        write_texture(&queue, texture, &palette, 256, 1, 4);
    }
    let tile = upload_tile(
        &device,
        &queue,
        NumericTile {
            width: 2,
            height: 2,
            values: vec![20.0, 80.0, 140.0, 200.0],
        },
    );
    let draw = pipeline.draw_tile(&device, (None, None), Some(&tile), Some(&tile));
    let mut data = uniform();
    data.extrusion = [1.0, 1.0, 0.5, 1.0];
    data.matrix[0][0] = 1.0;
    data.matrix[1][1] = -1.0;
    data.matrix[2][1] = 0.5;
    data.matrix[3][0] = -0.5;
    data.matrix[3][1] = 0.25;
    data.uv = [0.5, 0.0, 0.5, 0.5];
    data.previous_uv = data.uv;
    for blend in [0.0, 0.5, 1.0] {
        data.blend = blend;
        queue.write_buffer(&draw.uniform, 0, bytemuck::bytes_of(&data));
        let image = paint_depth(&device, &queue, |pass| {
            pass.set_pipeline(&pipeline.extrusion_pipeline);
            pass.set_bind_group(0, &draw.bindings, &[]);
            draw_pixels(pass, data.extrusion);
        })?;
        assert_eq!(image.get_pixel(32, 22).0, [80, 80, 80, 255], "Raised top must extend beyond ground footprint");
        assert_eq!(image.get_pixel(32, 54).0, [80, 80, 80, 255], "Wall must retain sampled pixel color");
    }
    Ok(())
}

// A maximum-height bar projects 16 pixels above the ground's top edge (row 32).
pub(crate) fn height_matrix() -> [[f32; 4]; 4] {
    [[1.0, 0.0, 0.0, 0.0], [0.0, -0.5, 0.0, 0.0], [0.0, 1.0, -0.2, 0.0], [-0.5, 0.0, 0.6, 1.0]]
}

pub(crate) fn top_row(image: &image::RgbaImage) -> Option<u32> {
    (0..image.height()).find(|&y| image.get_pixel(32, y)[3] != 0)
}

fn height_pipeline(device: &wgpu::Device, queue: &wgpu::Queue) -> RasterPipeline {
    let pipeline = RasterPipeline::new(device, (0x16, 0x30, false));
    let palette: Vec<u8> = (0..=255).flat_map(|v| [v, v, v, 255]).collect();
    for texture in &pipeline.palettes {
        write_texture(queue, texture, &palette, 256, 1, 4);
    }
    pipeline
}

fn render_heights(device: &wgpu::Device, queue: &wgpu::Queue, pipeline: &RasterPipeline, data: DrawUniform, values: [f32; 2]) -> Result<image::RgbaImage> {
    let textures = values.map(|value| {
        upload_tile(
            device,
            queue,
            NumericTile {
                width: 1,
                height: 1,
                values: vec![value],
            },
        )
    });
    let draw = pipeline.draw_tile(device, (None, None), Some(&textures[0]), Some(&textures[1]));
    queue.write_buffer(&draw.uniform, 0, bytemuck::bytes_of(&data));
    paint_depth(device, queue, |pass| {
        pass.set_pipeline(if data.extrusion[3] > 0.0 {
            &pipeline.extrusion_pipeline
        } else {
            &pipeline.pipeline
        });
        pass.set_bind_group(0, &draw.bindings, &[]);
        draw_pixels(pass, data.extrusion);
    })
}

fn height_uniform() -> DrawUniform {
    DrawUniform {
        matrix: height_matrix(),
        extrusion: [1.0, 1.0, 0.5, 1.0],
        ..uniform()
    }
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_height_uses_min_mid_max_custom_range_and_scale_offset() -> Result<()> {
    let (device, queue) = device()?;
    let pipeline = height_pipeline(&device, &queue);
    let cases = [
        (10.0, [30.0, 50.0], [2.0, 10.0], 32, 0),
        (15.0, [30.0, 50.0], [2.0, 10.0], 24, 128),
        (20.0, [30.0, 50.0], [2.0, 10.0], 16, 255),
        (10.0, [20.0, 60.0], [2.0, 10.0], 28, 64),
        (20.0, [20.0, 60.0], [2.0, 10.0], 20, 191),
        (20.0, [10.0, 30.0], [-2.0, 50.0], 32, 0),
        (10.0, [10.0, 30.0], [-2.0, 50.0], 16, 255),
    ];
    for (value, range, scale_offset, row, color) in cases {
        let data = DrawUniform {
            range,
            scale_offset,
            ..height_uniform()
        };
        let image = render_heights(&device, &queue, &pipeline, data, [value; 2])?;
        assert_eq!(top_row(&image), Some(row), "value={value}, range={range:?}, scale={scale_offset:?}");
        assert_eq!(image.get_pixel(32, row).0, [color, color, color, 255]);
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_height_clamps_or_omits_and_zero_maximum_matches_flat_rendering() -> Result<()> {
    let (device, queue) = device()?;
    let pipeline = height_pipeline(&device, &queue);
    for (value, row) in [(-10.0, 32), (300.0, 16)] {
        let mut data = height_uniform();
        data.clamp_colors = [1; 4];
        assert_eq!(top_row(&render_heights(&device, &queue, &pipeline, data, [value; 2])?), Some(row));
        data.clamp_colors = [0; 4];
        assert_eq!(top_row(&render_heights(&device, &queue, &pipeline, data, [value; 2])?), None);
    }
    let mut data = height_uniform();
    data.extrusion[2] = 0.0;
    let collapsed = render_heights(&device, &queue, &pipeline, data, [255.0; 2])?;
    assert_eq!(top_row(&collapsed), Some(32));
    data.extrusion[3] = 0.0;
    assert_eq!(collapsed, render_heights(&device, &queue, &pipeline, data, [255.0; 2])?);
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_height_fades_each_snapshots_actual_height() -> Result<()> {
    let (device, queue) = device()?;
    let pipeline = height_pipeline(&device, &queue);
    // Current: scaled 40 in 30..50 => 1/2 of maximum 1/2 => 1/4.
    // Previous: scaled 80 in 40..80 => full maximum 1/4 => 1/4.
    // Both endpoints must stay at row 24, not multiply blended factors.
    let mut data = DrawUniform {
        range: [30.0, 50.0],
        scale_offset: [2.0, 10.0],
        previous_range: [40.0, 80.0],
        previous_scale_offset: [3.0, 20.0],
        previous_extrusion_height: 0.25,
        ..height_uniform()
    };
    for blend in [0.0, 0.25, 0.5, 0.75, 1.0] {
        data.blend = blend;
        assert_eq!(top_row(&render_heights(&device, &queue, &pipeline, data, [15.0, 20.0])?), Some(24));
    }
    data.previous_extrusion_height = 0.5;
    for (blend, row) in [(0.0, 16), (0.5, 20), (1.0, 24)] {
        data.blend = blend;
        assert_eq!(top_row(&render_heights(&device, &queue, &pipeline, data, [15.0, 20.0])?), Some(row));
    }
    Ok(())
}
