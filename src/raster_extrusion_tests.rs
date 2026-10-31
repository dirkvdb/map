use super::*;
use crate::raster_playback::{PlaybackRequest, PlaybackStyle};
use crate::raster_tiles::test_support::{device, parameters};

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_static_opaque_bypass_releases_targets_without_reloading_tiles() -> Result<()> {
    let (_directory, bands) = crate::raster_source::test_support::cog_fixture(1)?;
    let source = bands[0].restyled(ColorMapPreset::Gray, (0.0, 255.0))?.with_extrusion(true, 2.0)?;
    let (device, queue) = device()?;
    let mut layer = RasterLayer::new(device.clone(), queue.clone())?;
    layer.set_reduce_motion(true);
    layer.set_source(Some(source.clone()), 1.0, 1);

    let mut parameters = parameters(0.0);
    parameters.depth_format = 0x30;
    parameters.projection[10] = -0.000001;
    parameters.projection[14] = 0.8;
    layer.coverage = layer.view_tiles(&parameters, &source, None).iter().map(|tile| tile.canonical()).collect();
    for tile in &layer.coverage {
        let texture = upload_tile(&device, &queue, sample_tile([2; 2], &[100.0; 4]));
        layer.cache_bytes += texture.bytes;
        layer.cache.insert(
            *tile,
            CachedTile {
                texture: Some(texture),
                used: 0,
            },
        );
    }
    let counts = (layer.requests, layer.uploads, layer.generation, layer.cache_bytes);
    for opacity in [1.0, 0.5, 1.0] {
        layer.set_source(Some(source.clone()), opacity, 1);
        paint_depth(&device, &queue, |pass| layer.render(pass, &parameters))?;
        assert_eq!(layer.composite.is_some(), opacity < 1.0);
        assert_eq!((layer.requests, layer.uploads, layer.generation, layer.cache_bytes), counts);
        assert_eq!(layer.cache.len(), layer.coverage.len());
    }
    assert_eq!(layer.pipeline.as_ref().unwrap().formats, (0x16, 0x30, false));
    layer.previous = Some(PreviousRaster { source, cache: HashMap::new() });
    assert!(layer.needs_compositing(), "Opaque palettes can still fade against missing endpoints");
    paint_depth(&device, &queue, |pass| layer.render(pass, &parameters))?;
    assert!(layer.composite.is_some());
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_static_toggle_animates_without_invalidating_raw_tiles() -> Result<()> {
    let (_directory, bands) = crate::raster_source::test_support::cog_fixture(1)?;
    let flat = bands[0].with_extrusion(false, 4.0)?.with_extrusion_averaging(true)?;
    let raised = flat.with_extrusion(true, 4.0)?;
    let (device, queue) = device()?;
    let mut layer = RasterLayer::new(device, queue)?;
    layer.set_source(Some(flat.clone()), 1.0, 1);
    let root = Tile { z: 0, x: 0, y: 0 };
    layer.cache.insert(root, CachedTile { texture: None, used: 0 });
    let generation = layer.generation;

    layer.set_source(Some(raised.clone()), 1.0, 1);
    let now = Instant::now();
    assert_eq!(layer.extrusion_animation.height(4.0), 0.0, "Enabling must start at ground level");
    assert!(layer.poll_tiles(), "An otherwise idle map must keep pumping the height transition");
    layer.extrusion_animation.advance(now + Duration::from_millis(90));
    assert!((0.0..4.0).contains(&layer.extrusion_animation.height(4.0)));
    layer.extrusion_animation.advance(now + Duration::from_secs(1));
    assert_eq!(layer.extrusion_animation.height(4.0), 4.0);

    layer.set_source(Some(flat.clone()), 1.0, 1);
    let now = Instant::now();
    assert_eq!(layer.extrusion_animation.height(0.0), 4.0, "Disabling must retain the visible height initially");
    assert_eq!(layer.extrusion_animation.maximum_height(0.0), 4.0, "Shrinking bars still need volume coverage");
    layer.extrusion_animation.advance(now + Duration::from_millis(90));
    assert!((0.0..4.0).contains(&layer.extrusion_animation.height(0.0)));
    layer.extrusion_animation.advance(now + Duration::from_secs(1));
    assert_eq!(layer.extrusion_animation.height(0.0), 0.0);
    assert!(!layer.poll_tiles(), "The transition must stop scheduling frames after reaching ground");
    assert_eq!(layer.generation, generation);
    assert!(layer.cache.contains_key(&root));
    assert!(layer.previous.is_none());
    assert_eq!(layer.tile_counts(), (0, 0));
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_static_toggle_honors_reduced_motion_and_clearing() -> Result<()> {
    let (_directory, bands) = crate::raster_source::test_support::cog_fixture(1)?;
    let flat = bands[0].with_extrusion(false, 4.0)?;
    let raised = flat.with_extrusion(true, 4.0)?;
    let (device, queue) = device()?;
    let mut layer = RasterLayer::new(device, queue)?;
    layer.set_source(Some(flat.clone()), 1.0, 1);
    layer.set_source(Some(raised.clone()), 1.0, 1);
    layer.set_reduce_motion(true);
    assert!(!layer.extrusion_animation.is_active());
    assert_eq!(layer.extrusion_animation.height(4.0), 4.0);
    layer.set_source(Some(flat), 1.0, 1);
    assert_eq!(layer.extrusion_animation.height(0.0), 0.0);
    layer.set_reduce_motion(false);
    layer.set_source(Some(raised), 1.0, 1);
    assert!(layer.extrusion_animation.is_active());
    layer.set_source(None, 1.0, 1);
    assert!(!layer.extrusion_animation.is_active());
    Ok(())
}

#[test]
fn extrusion_snapshots_preserve_style_and_raw_identity() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("extrusion.tif");
    crate::raster_source::test_support::write_byte_raster(&path)?;
    let source = RasterSource::open(&path)?.remove(0);
    assert_eq!(
        (source.is_extruded(), source.extrusion_height()),
        (false, crate::raster_source::MAX_EXTRUSION_HEIGHT / 2.0)
    );
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
fn extrusion_height_normalizes_and_sampling_grid_does_not_change_height() -> Result<()> {
    let (_directory, sources) = crate::raster_source::test_support::cog_fixture(1)?;
    for (input, expected) in [(-1.0, 0.0), (11.0, 11.0), (20.0, 20.0), (21.0, 20.0), (f32::NAN, 10.0), (f32::INFINITY, 10.0)] {
        assert_eq!(PlaybackStyle::default().with_extrusion(true, input).extrusion(), Some((true, expected)));
        assert_eq!(sources[0].with_extrusion(true, input)?.extrusion_height(), expected);
    }
    let full = [0.0, 0.0, 1.0, 1.0];
    let quarter = [0.5, 0.0, 0.5, 0.5];
    assert_eq!(extrusion_grid(Some([256; 2]), quarter, None, full, 1.0), [128; 2]);
    assert_eq!(extrusion_grid(Some([256; 2]), full, Some([256; 2]), quarter, 0.0), [128; 2]);
    let p = parameters(2.0);
    assert_eq!(
        extrusion_uniform(&p, &sources[0], [256; 2], 20.0)[2],
        extrusion_uniform(&p, &sources[0], [256; 2], 10.0)[2] * 2.0
    );
    let reference = crate::raster_tiles::extrusion_height::maximum(&p, &sources[0], 3.0) as f32;
    for grid in [[128; 2], [256; 2], [512; 2], [1; 2]] {
        let uniform = extrusion_uniform(&p, &sources[0], grid, 3.0);
        assert_eq!(uniform[2], reference, "Cached, coarse and HiDPI grids must share one height scale");
        assert_eq!(&uniform[..2], &[grid[0] as f32, grid[1] as f32]);
    }
    Ok(())
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
        geometry: [0; 4],
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
                draw_pixels(pass, uniform().extrusion, uniform().geometry, &pipeline.extrusion_indices);
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
            draw_pixels(pass, data.extrusion, data.geometry, &pipeline.extrusion_indices);
        })?;
        assert_eq!(image.get_pixel(32, 22).0, [80, 80, 80, 255], "Raised top must extend beyond ground footprint");
        assert_eq!(image.get_pixel(32, 54).0, [26, 26, 26, 255], "South wall must shade the sampled pixel color");
    }
    Ok(())
}

// A maximum-height bar projects 16 pixels above the ground's top edge (row 32).
pub(crate) fn height_matrix() -> [[f32; 4]; 4] {
    [[1.0, 0.0, 0.0, 0.0], [0.0, -0.5, 0.0, 0.0], [0.0, 1.0, -0.2, 0.0], [-0.5, 0.0, 0.6, 1.0]]
}

// Rotate the local unit square about its center, retaining height_matrix's projection.
pub(crate) fn rotated_height_matrix(azimuth: usize) -> [[f32; 4]; 4] {
    let (cos, sin) = [(1.0, 0.0), (0.0, 1.0), (-1.0, 0.0), (0.0, -1.0)][azimuth];
    [
        [cos, -0.5 * sin, 0.0, 0.0],
        [-sin, -0.5 * cos, 0.0, 0.0],
        [0.0, 1.0, -0.2, 0.0],
        [-0.5 * cos + 0.5 * sin, -0.25 + 0.25 * sin + 0.25 * cos, 0.6, 1.0],
    ]
}

pub(crate) fn write_solid_palette(queue: &wgpu::Queue, texture: &wgpu::Texture, color: [u8; 4]) {
    write_texture(queue, texture, &color.repeat(256), 256, 1, 4);
}

pub(crate) fn assert_bar_faces(image: &image::RgbaImage, color: [u8; 4], wall: &str) {
    // The light is fixed in map coordinates, not attached to the camera.
    let illumination = match wall {
        "south" => 0.32,
        "east" => 0.42,
        "north" => 0.88,
        "west" => 0.78,
        _ => panic!("Unknown wall: {wall}"),
    };
    // Sample both triangles of each face, away from shared edges.
    for x in [24, 40] {
        assert_eq!(image.get_pixel(x, 24).0, color, "{wall} view, top at x={x}");
        let shaded = image.get_pixel(x, 40).0;
        assert_eq!(shaded[3], color[3], "Lighting must not change alpha");
        for channel in 0..3 {
            let expected = (f32::from(color[channel]) * illumination).round() as u8;
            assert!(shaded[channel].abs_diff(expected) <= 1, "{wall} wall at x={x}: {shaded:?}");
        }
        assert!(shaded[2] < color[2], "Wall must be darker than the top");
    }
    for (x, y) in [(8, 32), (56, 32), (32, 8), (32, 56)] {
        assert_eq!(image.get_pixel(x, y).0, [0; 4], "outside {wall} bar at ({x}, {y})");
    }
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

pub(crate) fn sample_tile(size: [u32; 2], values: &[f32]) -> NumericTile {
    NumericTile {
        width: size[0],
        height: size[1],
        values: values.to_vec(),
    }
}

fn render_heights(device: &wgpu::Device, queue: &wgpu::Queue, pipeline: &RasterPipeline, data: DrawUniform, values: [f32; 2]) -> Result<image::RgbaImage> {
    render_tiles(device, queue, pipeline, data, values.map(|value| sample_tile([1; 2], &[value])))
}

fn render_tiles(device: &wgpu::Device, queue: &wgpu::Queue, pipeline: &RasterPipeline, data: DrawUniform, tiles: [NumericTile; 2]) -> Result<image::RgbaImage> {
    let textures = tiles.map(|tile| upload_tile(device, queue, tile));
    let draw = pipeline.draw_tile(device, (None, None), Some(&textures[0]), Some(&textures[1]));
    queue.write_buffer(&draw.uniform, 0, bytemuck::bytes_of(&data));
    paint_depth(device, queue, |pass| {
        pass.set_pipeline(if data.extrusion[3] > 0.0 {
            &pipeline.extrusion_pipeline
        } else {
            &pipeline.pipeline
        });
        pass.set_bind_group(0, &draw.bindings, &[]);
        draw_pixels(pass, data.extrusion, data.geometry, &pipeline.extrusion_indices);
    })
}

pub(crate) fn odd_averaging_values() -> ([f32; 15], [f32; 15]) {
    (
        [-100.0, 0.0, 10.0, 30.0, 40.0, 0.0, 100.0, 50.0, 70.0, 80.0, 20.0, 60.0, 90.0, 110.0, 200.0],
        [0.0, 0.0, 40.0, 40.0, 60.0, 0.0, 0.0, 40.0, 40.0, 60.0, 40.0, 40.0, 100.0, 100.0, 200.0],
    )
}

fn averaging_uniform() -> DrawUniform {
    DrawUniform {
        range: [0.0, 100.0],
        previous_range: [0.0, 100.0],
        extrusion: [2.0, 2.0, 0.5, 1.0],
        geometry: [2, 2, 0, 3],
        ..height_uniform()
    }
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
fn extrusion_indices_wind_top_and_four_walls_outward() {
    assert_eq!(EXTRUSION_INDICES.len(), 30, "Five faces, two triangles per face");
    let corners: std::collections::BTreeSet<_> = EXTRUSION_INDICES.into_iter().collect();
    assert_eq!(corners, (0..8).collect(), "Eight distinct bit-coded corners");
    // The projection reflects south-positive tile Y, so outward faces use FrontFace::Cw.
    let normals = [[0, 0, 1], [0, -1, 0], [1, 0, 0], [0, 1, 0], [-1, 0, 0]];
    let face_corners = [[4, 5, 6, 7], [0, 1, 4, 5], [1, 3, 5, 7], [2, 3, 6, 7], [0, 2, 4, 6]];
    for ((face, expected), corners) in EXTRUSION_INDICES.as_chunks::<6>().0.iter().zip(normals).zip(face_corners) {
        let actual: std::collections::BTreeSet<_> = face.iter().copied().collect();
        assert_eq!(actual, corners.into_iter().collect(), "face normal={expected:?}");
        let shared: Vec<_> = face[..3].iter().filter(|index| face[3..].contains(index)).copied().collect();
        assert_eq!(shared.len(), 2);
        assert_eq!((shared[0] ^ shared[1]).count_ones(), 2, "Triangles must share a face diagonal");
        for triangle in face.as_chunks::<3>().0 {
            assert!(triangle.iter().all(|&index| index < 8));
            let [a, b, c] = [triangle[0], triangle[1], triangle[2]].map(|index| [(index & 1) as i32, ((index >> 1) & 1) as i32, ((index >> 2) & 1) as i32]);
            let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
            let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
            let normal = [ab[1] * ac[2] - ab[2] * ac[1], ab[2] * ac[0] - ab[0] * ac[2], ab[0] * ac[1] - ab[1] * ac[0]];
            assert_eq!(normal, expected, "triangle={triangle:?}");
        }
    }
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_endpoints_ignore_unused_nodata_and_keep_snapshot_style() -> Result<()> {
    let (device, queue) = device()?;
    let pipeline = height_pipeline(&device, &queue);
    let current: Vec<u8> = (0..=255).flat_map(|v| [v, 0, 255 - v, 255]).collect();
    let previous: Vec<u8> = (0..=255).flat_map(|v| [0, 255 - v, v, 255]).collect();
    write_texture(&queue, &pipeline.palettes[0], &current, 256, 1, 4);
    write_texture(&queue, &pipeline.palettes[1], &previous, 256, 1, 4);
    // GpuReadback stores native-host BGRA bytes in RgbaImage; expectations use BGRA.
    for (blend, values, row, color) in [
        (0.0, [f32::NAN, 20.0], 26, [191, 64, 0, 255]),
        (0.0, [15.0, 20.0], 26, [191, 64, 0, 255]),
        (1.0, [15.0, f32::NAN], 28, [191, 0, 64, 255]),
        (1.0, [15.0, 20.0], 28, [191, 0, 64, 255]),
    ] {
        let data = DrawUniform {
            blend,
            range: [30.0, 70.0],
            scale_offset: [2.0, 10.0],
            previous_range: [20.0, 100.0],
            previous_scale_offset: [3.0, 20.0],
            previous_extrusion_height: 0.25,
            ..height_uniform()
        };
        let image = render_heights(&device, &queue, &pipeline, data, values)?;
        assert_eq!(top_row(&image), Some(row), "blend={blend}, values={values:?}");
        assert_eq!(image.get_pixel(32, row).0, color, "blend={blend}, values={values:?}");
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_zero_height_matches_flat_with_masked_endpoints_and_blends() -> Result<()> {
    let (device, queue) = device()?;
    let pipeline = height_pipeline(&device, &queue);
    for (values, alphas) in [
        ([15.0, 20.0], [255, 255, 255]),
        ([f32::NAN, 20.0], [255, 128, 0]),
        ([15.0, f32::NAN], [0, 128, 255]),
        ([f32::NAN; 2], [0, 0, 0]),
    ] {
        for (blend, alpha) in [0.0, 0.5, 1.0].into_iter().zip(alphas) {
            let mut data = DrawUniform {
                blend,
                range: [30.0, 70.0],
                scale_offset: [2.0, 10.0],
                previous_range: [20.0, 100.0],
                previous_scale_offset: [3.0, 20.0],
                previous_extrusion_height: 0.0,
                extrusion: [1.0, 1.0, 0.0, 1.0],
                ..height_uniform()
            };
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
fn gpu_extrusion_lighting_preserves_top_colors_and_shades_walls_at_four_azimuths() -> Result<()> {
    let (device, queue) = device()?;
    let pipeline = height_pipeline(&device, &queue);
    write_solid_palette(&queue, &pipeline.palettes[0], [192, 128, 64, 128]);
    for (azimuth, wall) in ["south", "east", "north", "west"].into_iter().enumerate() {
        let data = DrawUniform {
            matrix: rotated_height_matrix(azimuth),
            opacity: 0.5,
            ..height_uniform()
        };
        let image = render_heights(&device, &queue, &pipeline, data, [255.0, f32::NAN])?;
        // Premultiplied BGRA, matching the native-host readback contract.
        assert_bar_faces(&image, [16, 32, 48, 64], wall);
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_endpoints_respect_the_selected_snapshots_clamp_flag() -> Result<()> {
    let (device, queue) = device()?;
    let pipeline = height_pipeline(&device, &queue);
    for (blend, values, row) in [(0.0, [f32::NAN, 40.0], 24), (1.0, [40.0, f32::NAN], 16)] {
        for clamp_colors in [[0, 1, 0, 0], [1, 0, 0, 0]] {
            let data = DrawUniform {
                blend,
                clamp_colors,
                range: [30.0, 70.0],
                scale_offset: [2.0, 10.0],
                previous_range: [20.0, 100.0],
                previous_scale_offset: [3.0, 20.0],
                previous_extrusion_height: 0.25,
                ..height_uniform()
            };
            let image = render_heights(&device, &queue, &pipeline, data, values)?;
            let clamped = clamp_colors[usize::from(blend == 0.0)] != 0;
            assert_eq!(top_row(&image), clamped.then_some(row), "blend={blend}, clamp={clamp_colors:?}");
            if clamped {
                assert_eq!(image.get_pixel(32, row).0, [255; 4]);
            } else {
                assert!(image.pixels().all(|pixel| pixel.0 == [0; 4]));
            }
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_traversal_flags_cover_an_odd_grid_without_changing_colors() -> Result<()> {
    let (device, queue) = device()?;
    let pipeline = height_pipeline(&device, &queue);
    let texture = upload_tile(
        &device,
        &queue,
        NumericTile {
            width: 5,
            height: 3,
            values: (0..15).map(|index| if index == 7 { f32::NAN } else { 20.0 + index as f32 * 10.0 }).collect(),
        },
    );
    let draw = pipeline.draw_tile(&device, (None, None), Some(&texture), None);
    let render = |flags| {
        let data = DrawUniform {
            extrusion: [5.0, 3.0, 0.5, 1.0],
            geometry: [1, 1, flags, 0],
            ..uniform()
        };
        queue.write_buffer(&draw.uniform, 0, bytemuck::bytes_of(&data));
        paint_depth(&device, &queue, |pass| {
            pass.set_pipeline(&pipeline.extrusion_pipeline);
            pass.set_bind_group(0, &draw.bindings, &[]);
            draw_pixels(pass, data.extrusion, data.geometry, &pipeline.extrusion_indices);
        })
    };
    let reference = render(0)?;
    for flags in 1..8 {
        assert_eq!(render(flags)?, reference, "Traversal flags {flags} must visit each cell exactly once");
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_lod_keeps_height_and_partial_edge_footprints() -> Result<()> {
    let (device, queue) = device()?;
    let pipeline = height_pipeline(&device, &queue);
    let mut data = DrawUniform {
        extrusion: [5.0, 3.0, 0.5, 1.0],
        ..height_uniform()
    };
    let reference = render_heights(&device, &queue, &pipeline, data, [255.0; 2])?;
    assert_eq!(geometry::instance_count(data.extrusion, [2, 2, 0, 0]), 6);
    for flags in 0..8 {
        data.geometry = [2, 2, flags, 0];
        let image = render_heights(&device, &queue, &pipeline, data, [255.0; 2])?;
        assert_eq!(top_row(&image), top_row(&reference), "LOD must not inflate bar height");
        for (x, y) in [(17, 20), (46, 20), (17, 46), (46, 46)] {
            assert_eq!(image.get_pixel(x, y), reference.get_pixel(x, y), "Edge coverage at ({x}, {y}), flags={flags}");
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_cached_ancestors_do_not_cap_fine_replacement_requests() -> Result<()> {
    let (_directory, bands) = crate::raster_source::test_support::cog_fixture(1)?;
    let source = bands[0].with_extrusion(true, 1.0)?;
    let (device, queue) = device()?;
    let mut layer = RasterLayer::new(device, queue)?;
    layer.set_source(Some(source.clone()), 1.0, 1);
    let p = parameters(5.0);
    let target = layer.view_tiles(&p, &source, None);
    assert!(!target.is_empty());
    assert!(target.iter().all(|tile| tile.z > 0));
    let root = Tile { z: 0, x: 0, y: 0 };
    let texture = upload_tile(
        &layer.device,
        &layer.queue,
        NumericTile {
            width: 256,
            height: 256,
            values: vec![100.0; 256 * 256],
        },
    );
    layer.cache.insert(
        root,
        CachedTile {
            texture: Some(texture),
            used: 0,
        },
    );
    assert_eq!(layer.view_tiles(&p, &source, None), target, "Unused ancestors must not change request detail");
    layer.prepare_pipeline(&p);
    let draw = layer
        .pipeline
        .as_ref()
        .unwrap()
        .draw_tile(&layer.device, (Some(root), None), layer.cache[&root].texture.as_ref(), None);
    layer.draws.insert(target[0], draw);
    assert_eq!(
        layer.view_tiles(&p, &source, None),
        target,
        "Actively sampled ancestors must not cause a coarse-grid feedback loop"
    );
    layer.request_visible_tiles(&target, false);
    assert!(
        layer.pending.iter().any(|tile| tile.z == target[0].z),
        "Fine requests must advance while the parent is displayed"
    );
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

#[test]
fn extrusion_averaging_counts_quarter_instances_and_partial_edge_blocks() {
    let full = [256.0, 256.0, 0.5, 1.0];
    assert_eq!(geometry::instance_count(full, [1, 1, 0, 0]), 256 * 256);
    assert_eq!(geometry::instance_count(full, [2, 2, 0, 3]), 256 * 256 / 4);
    assert_eq!(geometry::instance_count([5.0, 3.0, 0.5, 1.0], [2, 2, 0, 3]), 6);
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_averaging_styles_raw_mean_before_clamping_and_palette_lookup() -> Result<()> {
    let (device, queue) = device()?;
    let pipeline = height_pipeline(&device, &queue);
    let palette: Vec<u8> = (0..=255).flat_map(|v| [v, 255 - v, if v < 128 { 0 } else { 255 }, 255]).collect();
    write_texture(&queue, &pipeline.palettes[0], &palette, 256, 1, 4);
    // BGRA readback; the nonlinear blue channel also rejects averaging palette colors.
    for (values, mean, row, color) in [
        ([-100.0, 0.0, 0.0, 100.0], 0.0, 32, [0, 255, 0, 255]),
        ([0.0, 0.0, 0.0, 100.0], 25.0, 28, [0, 191, 64, 255]),
        ([-100.0, 0.0, 100.0, 200.0], 50.0, 24, [255, 127, 128, 255]),
    ] {
        for clamp in [0, 1] {
            let mut data = averaging_uniform();
            data.clamp_colors = [clamp; 4];
            let tiles = [sample_tile([2; 2], &values), sample_tile([1; 2], &[f32::NAN])];
            let image = render_tiles(&device, &queue, &pipeline, data, tiles)?;
            assert_eq!(top_row(&image), Some(row), "values={values:?}, clamp={clamp}");
            assert_eq!(image.get_pixel(32, row).0, color);
            data.geometry[3] = 0;
            assert_eq!(image, render_heights(&device, &queue, &pipeline, data, [mean, f32::NAN])?);
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_averaging_ignores_nonfinite_contributors_without_reducing_alpha() -> Result<()> {
    let (device, queue) = device()?;
    let pipeline = height_pipeline(&device, &queue);
    let reference = render_heights(&device, &queue, &pipeline, averaging_uniform(), [50.0; 2])?;
    for values in [
        [50.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY],
        [0.0, 100.0, f32::NAN, f32::INFINITY],
        [0.0, 50.0, 100.0, f32::NAN],
    ] {
        let tiles = [sample_tile([2; 2], &values), sample_tile([1; 2], &[f32::NAN])];
        let image = render_tiles(&device, &queue, &pipeline, averaging_uniform(), tiles)?;
        assert_eq!(top_row(&image), Some(24));
        assert_eq!(image.get_pixel(32, 24)[3], 255);
        // Dividing a three-contributor mean may round by one ULP on the GPU,
        // placing an exact palette midpoint on either adjacent LUT entry.
        assert!(
            image
                .pixels()
                .zip(reference.pixels())
                .all(|(actual, expected)| { actual[3] == expected[3] && actual.0[..3].iter().zip(&expected.0[..3]).all(|(a, b)| a.abs_diff(*b) <= 1) }),
            "values={values:?}"
        );
    }
    let invalid = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::NAN];
    let tiles = [sample_tile([2; 2], &invalid), sample_tile([1; 2], &[50.0])];
    let image = render_tiles(&device, &queue, &pipeline, averaging_uniform(), tiles)?;
    assert!(image.pixels().all(|pixel| pixel.0 == [0; 4]), "An all-nodata selected block must be omitted");
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_averaging_covers_odd_grid_edges_once_in_every_traversal() -> Result<()> {
    let (device, queue) = device()?;
    let pipeline = height_pipeline(&device, &queue);
    let (values, means) = odd_averaging_values();
    let mut data = DrawUniform {
        range: [0.0, 200.0],
        opacity: 0.5,
        extrusion: [5.0, 3.0, 0.5, 1.0],
        geometry: [1, 1, 0, 0],
        ..uniform()
    };
    let reference = render_tiles(
        &device,
        &queue,
        &pipeline,
        data,
        [sample_tile([5, 3], &means), sample_tile([1; 2], &[f32::NAN])],
    )?;
    assert!(reference.pixels().all(|pixel| pixel[3] == 128));
    for order in 0..8 {
        data.geometry = [2, 2, order, 3];
        let tiles = [sample_tile([5, 3], &values), sample_tile([1; 2], &[f32::NAN])];
        let image = render_tiles(&device, &queue, &pipeline, data, tiles)?;
        assert_eq!(image, reference, "Odd edge means, coverage and alpha must match: order={order}");
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_averaging_flags_select_each_snapshots_mean_before_style_and_height_fade() -> Result<()> {
    let (device, queue) = device()?;
    let pipeline = height_pipeline(&device, &queue);
    let current: Vec<u8> = (0..=255).flat_map(|v| [v, 0, 255 - v, 255]).collect();
    let previous: Vec<u8> = (0..=255).flat_map(|v| [0, 255 - v, v, 255]).collect();
    write_texture(&queue, &pipeline.palettes[0], &current, 256, 1, 4);
    write_texture(&queue, &pipeline.palettes[1], &previous, 256, 1, 4);
    for (flags, samples, rows) in [
        (0, [30.0, 40.0], [24, 20, 16]),
        (1, [30.0, 20.0], [28, 22, 16]),
        (2, [15.0, 40.0], [24, 26, 28]),
        (3, [15.0, 20.0], [28, 28, 28]),
    ] {
        for (blend, row) in [0.0, 0.5, 1.0].into_iter().zip(rows) {
            let mut data = DrawUniform {
                blend,
                range: [30.0, 70.0],
                scale_offset: [2.0, 10.0],
                previous_range: [0.0, 40.0],
                previous_extrusion_height: 0.25,
                geometry: [2, 2, 0, flags],
                ..averaging_uniform()
            };
            let tiles = [sample_tile([2; 2], &[0.0, 10.0, 20.0, 30.0]), sample_tile([2; 2], &[0.0, 20.0, 20.0, 40.0])];
            let image = render_tiles(&device, &queue, &pipeline, data, tiles)?;
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
fn gpu_extrusion_averaging_maps_each_parent_uv_sample_and_fades_a_coarser_snapshot() -> Result<()> {
    let (device, queue) = device()?;
    let pipeline = height_pipeline(&device, &queue);
    let logical = [
        -100.0, 0.0, 20.0, 40.0, 0.0, 100.0, 60.0, 80.0, 50.0, 70.0, 80.0, 100.0, 90.0, 110.0, 120.0, 140.0,
    ];
    let means = [0.0, 0.0, 50.0, 50.0, 0.0, 0.0, 50.0, 50.0, 80.0, 80.0, 110.0, 110.0, 80.0, 80.0, 110.0, 110.0];
    let mut parent = [1000.0; 64];
    for row in 0..4 {
        parent[row * 8 + 4..row * 8 + 8].copy_from_slice(&logical[row * 4..row * 4 + 4]);
    }
    for blend in [0.0, 0.5, 1.0] {
        let mut data = DrawUniform {
            blend,
            range: [0.0, 200.0],
            previous_range: [10.0, 210.0],
            previous_scale_offset: [2.0, 10.0],
            previous_extrusion_height: 0.25,
            uv: [0.5, 0.0, 0.5, 0.5],
            extrusion: [4.0, 4.0, 0.5, 1.0],
            ..averaging_uniform()
        };
        let tiles = [sample_tile([8; 2], &parent), sample_tile([2; 2], &[20.0, 40.0, 60.0, 80.0])];
        let image = render_tiles(&device, &queue, &pipeline, data, tiles)?;
        data.geometry[3] = 0;
        data.uv = [0.0, 0.0, 1.0, 1.0];
        let reference = [sample_tile([4; 2], &means), sample_tile([2; 2], &[20.0, 40.0, 60.0, 80.0])];
        assert_eq!(image, render_tiles(&device, &queue, &pipeline, data, reference)?, "blend={blend}");
    }
    Ok(())
}

#[test]
#[ignore = "requires a native graphics device"]
fn gpu_extrusion_averaging_flags_leave_flat_nearest_samples_unchanged() -> Result<()> {
    let (device, queue) = device()?;
    let pipeline = height_pipeline(&device, &queue);
    let mut data = DrawUniform {
        matrix: uniform().matrix,
        extrusion: [2.0, 2.0, 0.5, 0.0],
        geometry: [2, 2, 0, 0],
        ..averaging_uniform()
    };
    let tiles = || [sample_tile([2; 2], &[0.0, 25.0, 75.0, 100.0]), sample_tile([1; 2], &[f32::NAN])];
    let reference = render_tiles(&device, &queue, &pipeline, data, tiles())?;
    for (x, y, color) in [(16, 16, 0), (48, 16, 64), (16, 48, 191), (48, 48, 255)] {
        assert_eq!(reference.get_pixel(x, y).0, [color, color, color, 255]);
    }
    for flags in 1..=3 {
        data.geometry[3] = flags;
        assert_eq!(render_tiles(&device, &queue, &pipeline, data, tiles())?, reference, "flags={flags}");
    }
    Ok(())
}
