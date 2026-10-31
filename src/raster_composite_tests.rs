use super::RasterComposite;
use crate::raster_layer::extrusion_tests::paint_depth;
use crate::raster_tiles::test_support::{device, paint};
use anyhow::Result;
use wgpu::{Device, Queue, RenderPass, TextureFormat};

const COLOR: TextureFormat = TextureFormat::Rgba8Unorm;
const DEPTH: TextureFormat = TextureFormat::Depth32Float;
const RED_NEAR: u32 = 0;
const BLUE_FAR: u32 = 1;
const GREEN_MIDDLE: u32 = 2;
const YELLOW_FRONT: u32 = 3;
const NODATA_FRONT: u32 = 4;
const CYAN_TIED: u32 = 5;
const RED_TOP_LEFT: u32 = 6;

// Every instance is a fullscreen triangle. Colors are already premultiplied.
const SURFACES: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) @interpolate(flat) surface: u32,
};
@vertex
fn vertex(@builtin(vertex_index) vertex: u32, @builtin(instance_index) surface: u32) -> VertexOutput {
    let depths = array<f32, 7>(0.2, 0.7, 0.5, 0.1, 0.1, 0.2, 0.2);
    let uv = vec2<f32>(f32((vertex << 1u) & 2u), f32(vertex & 2u));
    var output: VertexOutput;
    output.position = vec4<f32>(uv * 2.0 - vec2<f32>(1.0), depths[surface], 1.0);
    output.surface = surface;
    return output;
}
@fragment
fn fragment(input: VertexOutput) -> @location(0) vec4<f32> {
    let colors = array<vec4<f32>, 7>(
        vec4<f32>(0.5, 0.0, 0.0, 0.5),
        vec4<f32>(0.0, 0.0, 1.0, 1.0),
        vec4<f32>(0.0, 1.0, 0.0, 1.0),
        vec4<f32>(1.0, 1.0, 0.0, 1.0),
        vec4<f32>(0.0),
        vec4<f32>(0.0, 1.0, 1.0, 1.0),
        vec4<f32>(0.5, 0.0, 0.0, 0.5),
    );
    if input.surface == 6u && (input.position.x >= 32.0 || input.position.y >= 32.0) {
        discard;
    }
    return colors[input.surface];
}
"#;

fn surface_pipeline(device: &Device) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("Composite regression surfaces"),
        source: wgpu::ShaderSource::Wgsl(SURFACES.into()),
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("Composite regression REPLACE + Less"),
        layout: None,
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vertex"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: Default::default(),
        depth_stencil: Some(wgpu::DepthStencilState {
            format: DEPTH,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::Less),
            stencil: Default::default(),
            bias: Default::default(),
        }),
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fragment"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: COLOR,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn draw(pass: &mut RenderPass<'_>, pipeline: &wgpu::RenderPipeline, surfaces: &[u32]) {
    pass.set_pipeline(pipeline);
    for &surface in surfaces {
        pass.draw(0..3, surface..surface + 1);
    }
}

fn populate(device: &Device, queue: &Queue, target: &RasterComposite, pipeline: &wgpu::RenderPipeline, surfaces: &[u32]) {
    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Composite regression shared target"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target.color,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &target.depth,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        draw(&mut pass, pipeline, surfaces);
    }
    queue.submit([encoder.finish()]);
}

fn assert_solid(image: &image::RgbaImage, mut expected: [u8; 4]) {
    // GpuReadback intentionally returns BGRA bytes inside RgbaImage; inputs here are RGBA.
    expected.swap(0, 2);
    for (x, y, pixel) in image.enumerate_pixels() {
        assert!(
            pixel.0.iter().zip(expected).all(|(&actual, expected)| actual.abs_diff(expected) <= 1),
            "pixel ({x}, {y}): expected {expected:?}, got {pixel:?}"
        );
    }
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device"]
fn shared_target_is_draw_order_invariant() -> Result<()> {
    let (device, queue) = device()?;
    let target = RasterComposite::new(&device, [64; 2], COLOR, Some(DEPTH));
    let pipeline = surface_pipeline(&device);
    populate(&device, &queue, &target, &pipeline, &[RED_NEAR, BLUE_FAR]);
    let near_first = paint_depth(&device, &queue, |pass| target.composite(pass, &queue, 0.5))?;
    populate(&device, &queue, &target, &pipeline, &[BLUE_FAR, RED_NEAR]);
    let far_first = paint_depth(&device, &queue, |pass| target.composite(pass, &queue, 0.5))?;
    assert_eq!(near_first, far_first, "Only the nearest surface should contribute, regardless of draw order");
    assert_solid(&near_first, [64, 0, 0, 64]);
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device"]
fn composite_applies_opacity_once_without_host_depth() -> Result<()> {
    let (device, queue) = device()?;
    let target = RasterComposite::new(&device, [64; 2], COLOR, None);
    let pipeline = surface_pipeline(&device);
    populate(&device, &queue, &target, &pipeline, &[BLUE_FAR, RED_NEAR]);
    for (opacity, expected) in [(1.0, [128, 0, 0, 128]), (0.5, [64, 0, 0, 64]), (0.0, [0; 4])] {
        let image = paint(&device, &queue, |pass| target.composite(pass, &queue, opacity))?;
        assert_solid(&image, expected);
    }
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device"]
fn nodata_preserves_host_color_and_does_not_write_depth() -> Result<()> {
    let (device, queue) = device()?;
    let target = RasterComposite::new(&device, [64; 2], COLOR, Some(DEPTH));
    let pipeline = surface_pipeline(&device);
    // Deliberately store zero-alpha color at depth 0.1, rather than merely clearing.
    populate(&device, &queue, &target, &pipeline, &[NODATA_FRONT]);
    let unchanged = paint_depth(&device, &queue, |pass| {
        draw(pass, &pipeline, &[BLUE_FAR]);
        target.composite(pass, &queue, 1.0);
    })?;
    assert_solid(&unchanged, [0, 0, 255, 255]);
    let later_geometry = paint_depth(&device, &queue, |pass| {
        draw(pass, &pipeline, &[BLUE_FAR]);
        target.composite(pass, &queue, 1.0);
        draw(pass, &pipeline, &[GREEN_MIDDLE]);
    })?;
    assert_solid(&later_geometry, [0, 255, 0, 255]);
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device"]
fn zero_opacity_does_not_occlude_later_host_geometry() -> Result<()> {
    let (device, queue) = device()?;
    let target = RasterComposite::new(&device, [64; 2], COLOR, Some(DEPTH));
    let pipeline = surface_pipeline(&device);
    populate(&device, &queue, &target, &pipeline, &[RED_NEAR]);
    let image = paint_depth(&device, &queue, |pass| {
        target.composite(pass, &queue, 0.0);
        draw(pass, &pipeline, &[GREEN_MIDDLE]);
    })?;
    assert_solid(&image, [0, 255, 0, 255]);
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device"]
fn composite_respects_host_depth_and_accepts_equal_depth() -> Result<()> {
    let (device, queue) = device()?;
    let target = RasterComposite::new(&device, [64; 2], COLOR, Some(DEPTH));
    let pipeline = surface_pipeline(&device);
    populate(&device, &queue, &target, &pipeline, &[RED_NEAR]);
    let cases = [
        (YELLOW_FRONT, [255, 255, 0, 255]),
        (GREEN_MIDDLE, [128, 128, 0, 255]),
        (CYAN_TIED, [128, 128, 128, 255]),
    ];
    for (host, expected) in cases {
        let image = paint_depth(&device, &queue, |pass| {
            draw(pass, &pipeline, &[host]);
            target.composite(pass, &queue, 1.0);
        })?;
        assert_solid(&image, expected);
    }
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device"]
fn composite_writes_sampled_depth_to_occlude_later_host_geometry() -> Result<()> {
    let (device, queue) = device()?;
    let target = RasterComposite::new(&device, [64; 2], COLOR, Some(DEPTH));
    let pipeline = surface_pipeline(&device);
    populate(&device, &queue, &target, &pipeline, &[RED_NEAR]);
    let image = paint_depth(&device, &queue, |pass| {
        draw(pass, &pipeline, &[BLUE_FAR]);
        target.composite(pass, &queue, 1.0);
        draw(pass, &pipeline, &[GREEN_MIDDLE]);
    })?;
    assert_solid(&image, [128, 0, 128, 255]);
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device"]
fn composite_preserves_spatial_nodata_and_screen_coordinates() -> Result<()> {
    let (device, queue) = device()?;
    let target = RasterComposite::new(&device, [64; 2], COLOR, Some(DEPTH));
    let pipeline = surface_pipeline(&device);
    populate(&device, &queue, &target, &pipeline, &[RED_TOP_LEFT]);
    let image = paint_depth(&device, &queue, |pass| {
        draw(pass, &pipeline, &[GREEN_MIDDLE]);
        target.composite(pass, &queue, 1.0);
    })?;
    for (x, y, pixel) in image.enumerate_pixels() {
        let mut expected = if x < 32 && y < 32 { [128, 128, 0, 255] } else { [0, 255, 0, 255] };
        // Match GpuReadback's BGRA byte order, despite the RgbaImage container.
        expected.swap(0, 2);
        assert!(
            pixel.0.iter().zip(expected).all(|(&actual, expected)| actual.abs_diff(expected) <= 1),
            "pixel ({x}, {y}): expected {expected:?}, got {pixel:?}"
        );
    }
    Ok(())
}

#[test]
#[ignore = "requires a wgpu-compatible graphics device"]
fn matches_reuses_only_identical_size_and_attachment_formats() -> Result<()> {
    let (device, _queue) = device()?;
    let target = RasterComposite::new(&device, [64; 2], COLOR, Some(DEPTH));
    assert!(target.matches([64; 2], COLOR, Some(DEPTH)));
    for (size, color, depth) in [
        ([32, 64], COLOR, Some(DEPTH)),
        ([64, 32], COLOR, Some(DEPTH)),
        ([64; 2], TextureFormat::Bgra8Unorm, Some(DEPTH)),
        ([64; 2], COLOR, Some(TextureFormat::Depth24Plus)),
        ([64; 2], COLOR, None),
    ] {
        assert!(!target.matches(size, color, depth), "Must rebuild for {size:?}, {color:?}, {depth:?}");
    }
    let resized = RasterComposite::new(&device, [32, 64], TextureFormat::Bgra8Unorm, None);
    assert!(resized.matches([32, 64], TextureFormat::Bgra8Unorm, None));
    assert!(!resized.matches([32, 64], TextureFormat::Bgra8Unorm, Some(DEPTH)));
    Ok(())
}
