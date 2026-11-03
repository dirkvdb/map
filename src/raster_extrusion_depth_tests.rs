//! Isolate globe volume Z rounding, not planet-depth interference or face lighting.
//!
//! Run with `cargo test extrusion_depth_tests -- --ignored --test-threads=1`.
//! The oracle shares production XYW, sampling, indices and shading; only its near
//! distance is enlarged. It is not an independent world-coordinate oracle.

use super::*;
use crate::raster_layer::extrusion_tests::paint_depth_with_clear;
use crate::raster_tiles::test_support::device;
use maplibre_native::{CameraUpdate, Continuous, ImageRenderer, ImageRendererBuilder, LatLng};

const CENTER: [f64; 2] = [10.0, 49.0];
const GOLD: [u8; 4] = [224, 176, 64, 255];
const GRID: u32 = 256;

struct CaptureLayer(Rc<RefCell<Option<WgpuLayerParameters>>>);

impl WgpuCustomLayer for CaptureLayer {
    fn render(&mut self, _: &mut wgpu::RenderPass<'_>, parameters: &WgpuLayerParameters) {
        *self.0.borrow_mut() = Some(parameters.clone());
    }
}

struct NativeCamera {
    renderer: ImageRenderer<Continuous>,
    captured: Rc<RefCell<Option<WgpuLayerParameters>>>,
}

impl NativeCamera {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Result<Self> {
        let captured = Rc::new(RefCell::new(None));
        // Keep native W at its real 512-pixel viewport scale, even though we
        // later crop XY to the central 64 pixels for inexpensive readbacks.
        let mut renderer = ImageRendererBuilder::new().build_continuous_renderer();
        renderer.set_device_queue(device.clone(), queue.clone());
        renderer.load_style_from_json_str(r#"{"version":8,"sources":{},"layers":[]}"#).wait()?;
        renderer.style().add_wgpu_custom_layer("capture", CaptureLayer(Rc::clone(&captured)), None)?;
        renderer.set_globe(true);
        Ok(Self { renderer, captured })
    }

    fn parameters(&mut self, zoom: f64, offset: f64) -> WgpuLayerParameters {
        let longitude = CENTER[0] + offset * 360.0 / (512.0 * zoom.exp2());
        self.renderer.update_camera(
            &CameraUpdate::new()
                .center(LatLng {
                    lat: CENTER[1],
                    lng: longitude,
                })
                .zoom(zoom)
                .pitch(55.0)
                .bearing(37.0 + offset * 0.01),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.captured.borrow_mut().take();
            self.renderer.render_once();
            if let Some(parameters) = self.captured.borrow_mut().take() {
                assert!(parameters.globe && parameters.projection_transition > 0.0);
                assert!((parameters.zoom - zoom).abs() < 1e-6);
                return parameters;
            }
            assert!(Instant::now() < deadline, "Native globe camera did not render");
        }
    }
}

fn depth_uniform(parameters: &WgpuLayerParameters, cell_pixels: f64) -> DrawUniform {
    let world = 512.0 * parameters.zoom.exp2();
    let latitude = CENTER[1].to_radians();
    let center = [CENTER[0] / 360.0 + 0.5, 0.5 - latitude.tan().asinh() / std::f64::consts::TAU];
    let span = f64::from(GRID) * cell_pixels / world;
    let mut projection = ProjectionUniform {
        globe_matrix: std::array::from_fn(|column| std::array::from_fn(|row| parameters.globe_projection[column * 4 + row] as f32)),
        clipping_plane: parameters.globe_clipping_plane.map(|value| value as f32),
        tile_mercator: [(center[0] - span / 2.0) as f32, (center[1] - span / 2.0) as f32, span as f32, span as f32],
        parameters: [parameters.projection_transition as f32, parameters.depth_offset as f32, 1.0, 1.0],
    };
    for column in &mut projection.globe_matrix {
        column[0] *= 8.0;
        column[1] *= 8.0;
    }
    // Values 249..255 produce positive heights differing by at most 0.25 world
    // pixel. Equal-height fields hide the wall/top depth winner regression.
    let height = (10.25 * std::f64::consts::TAU * 6_371_008.8 * latitude.cos() / world) as f32;
    // Match tile_matrix for this arbitrary Mercator footprint, including the
    // same central-viewport crop as the globe matrix during projection blends.
    let matrix = std::array::from_fn(|column| {
        std::array::from_fn(|row| {
            let p = &parameters.projection;
            let value = match column {
                0 | 1 => p[column * 4 + row] * world * span,
                2 => p[8 + row],
                _ => p[row] * world * (center[0] - span / 2.0) + p[4 + row] * world * (center[1] - span / 2.0) + p[12 + row],
            };
            (value * if row < 2 { 8.0 } else { 1.0 }) as f32
        })
    });
    DrawUniform {
        matrix,
        uv: [0.0, 0.0, 1.0, 1.0],
        previous_uv: [0.0, 0.0, 1.0, 1.0],
        range: [0.0, 255.0],
        previous_range: [0.0, 255.0],
        scale_offset: [1.0, 0.0],
        previous_scale_offset: [1.0, 0.0],
        opacity: 1.0,
        blend: 1.0,
        tile_size: GRID as f32,
        extrusion: [GRID as f32, GRID as f32, height, 1.0],
        geometry: [1, 1, 0, 0],
        projection,
        ..DrawUniform::zeroed()
    }
}

fn varying_heights(device: &wgpu::Device, queue: &wgpu::Queue) -> TileTexture {
    upload_tile(
        device,
        queue,
        NumericTile {
            width: GRID,
            height: GRID,
            values: (0..GRID * GRID).map(|index| 249.0 + ((index * 17 + (index / GRID) * 13) % 7) as f32).collect(),
        },
    )
}

fn oracle_pipeline(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, near: f32) -> wgpu::RenderPipeline {
    let projection = include_str!("raster_projection.wgsl").replace("fn project_volume(", "fn native_project_volume(");
    // Never use native_project_volume's Z: independently accumulated global Z
    // and W can cancel differently. W-near is monotonic in camera distance.
    let wrapper = format!(
        "fn project_volume(uv: vec2<f32>, height: f32) -> vec4<f32> {{\n\
         var position = native_project_volume(uv, height);\n\
         if raster_projection_active() {{ position.z = position.w - {near:.1}; }}\n\
         return position;\n}}"
    );
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("Globe volume W-depth oracle"),
        source: wgpu::ShaderSource::Wgsl(
            format!(
                "{projection}\n{wrapper}\n{}\n{}\n{}",
                include_str!("raster_sample.wgsl"),
                include_str!("raster_extrusion_sample.wgsl"),
                include_str!("raster_extrusion.wgsl"),
            )
            .into(),
        ),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("Globe volume oracle layout"),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("Globe volume oracle"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("extruded_vertex"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState {
            front_face: wgpu::FrontFace::Cw,
            cull_mode: Some(wgpu::Face::Back),
            ..Default::default()
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::Less),
            stencil: Default::default(),
            bias: Default::default(),
        }),
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("extruded_fragment"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba8Unorm,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn render_depth(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &wgpu::RenderPipeline,
    resources: &RasterPipeline,
    draw: &DrawTile,
    uniform: &DrawUniform,
    clear: f32,
) -> Result<image::RgbaImage> {
    queue.write_buffer(&draw.uniform, 0, bytemuck::bytes_of(uniform));
    paint_depth_with_clear(device, queue, clear, |pass| {
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &draw.bindings, &[]);
        draw_pixels(pass, uniform.extrusion, uniform.geometry, &resources.extrusion_indices, uniform.projection);
    })
}

fn color_mismatches(actual: &image::RgbaImage, expected: &image::RgbaImage) -> usize {
    actual
        .pixels()
        .zip(expected.pixels())
        .filter(|(actual, expected)| actual.0.into_iter().zip(expected.0).any(|(a, b)| a.abs_diff(b) > 3))
        .count()
}

fn assert_oracle_near_plane_cannot_clip(uniform: &DrawUniform) {
    let projection = &uniform.projection;
    let mercator = projection.tile_mercator.map(f64::from);
    let matrix = projection.globe_matrix;
    let mut minimum_w = f64::INFINITY;
    // Height is affine in the sphere radius. Ground and maximum height bound
    // every bar corner; triangle interpolation cannot lower W below its vertices.
    for index in 0..(GRID + 1).pow(2) {
        let x = mercator[0] + f64::from(index % (GRID + 1)) / f64::from(GRID) * mercator[2];
        let y = mercator[1] + f64::from(index / (GRID + 1)) / f64::from(GRID) * mercator[3];
        let longitude = x * std::f64::consts::TAU + std::f64::consts::PI;
        let t = (std::f64::consts::PI - y * std::f64::consts::TAU).exp();
        let cos_latitude = 2.0 * t / (t * t + 1.0);
        let sphere = [longitude.sin() * cos_latitude, (t * t - 1.0) / (t * t + 1.0), longitude.cos() * cos_latitude];
        let radial_w: f64 = sphere.into_iter().zip(matrix).map(|(component, column)| component * f64::from(column[3])).sum();
        let translation = f64::from(matrix[3][3]);
        let radius = 1.0 + f64::from(uniform.extrusion[2]) / 6_371_008.8;
        let globe_w = [radial_w + translation, radial_w * radius + translation];
        let uv = [f64::from(index % (GRID + 1)), f64::from(index / (GRID + 1))].map(|value| value / f64::from(GRID));
        let flat_w = f64::from(uniform.matrix[0][3]) * uv[0] + f64::from(uniform.matrix[1][3]) * uv[1] + f64::from(uniform.matrix[3][3]);
        let transition = f64::from(projection.parameters[0]);
        for (height, globe_w) in [0.0, f64::from(uniform.extrusion[2])].into_iter().zip(globe_w) {
            let w = if transition > 0.999 {
                globe_w
            } else {
                (flat_w + height * f64::from(uniform.matrix[2][3])) * (1.0 - transition) + globe_w * transition
            };
            minimum_w = minimum_w.min(w);
        }
    }
    // A whole clip-unit margin dwarfs global-matrix f32 rounding in this fixture.
    assert!(minimum_w > 129.0, "W-128 oracle would clip fixture geometry: minimum W={minimum_w}");
}

fn assert_visible_faces(image: &image::RgbaImage) {
    // GpuReadback returns native-host BGRA bytes, despite its RgbaImage type.
    let top = [GOLD[2], GOLD[1], GOLD[0], GOLD[3]];
    let tops = image.pixels().filter(|pixel| pixel.0 == top).count();
    let walls = image.pixels().filter(|pixel| pixel[3] == 255 && pixel[2] + 8 < top[2]).count();
    assert!(
        tops > 500 && walls > 20,
        "Fixture must expose both gold tops and brown walls: tops={tops}, walls={walls}"
    );
}

#[test]
#[ignore = "requires a native graphics device; local empty style, no network"]
fn gpu_globe_varying_nonzero_heights_match_high_precision_w_depth() -> Result<()> {
    let (device, queue) = device()?;
    let mut camera = NativeCamera::new(&device, &queue)?;
    let resources = RasterPipeline::new(&device, (0x16, 0x30, false, true));
    let palette: Vec<_> = (0..256).flat_map(|_| GOLD).collect();
    write_texture(&queue, &resources.palettes[0], &palette, 256, 1, 4);
    let tile = varying_heights(&device, &queue);
    let draw = resources.draw_tile(&device, (None, None), Some(&tile), None);
    let oracle = oracle_pipeline(&device, &resources.layout, 128.0);
    let control = oracle_pipeline(&device, &resources.layout, 32.0);
    // Include both sides of the full-globe snap and near-completed blends.
    for (zoom, cell_pixels) in [(8.0, 0.5), (10.0, 1.0), (11.0, 2.0), (11.002, 2.0), (11.5, 2.0), (11.99, 2.0)] {
        for offset in [-0.125, 0.0, 0.125] {
            let parameters = camera.parameters(zoom, offset);
            let uniform = depth_uniform(&parameters, cell_pixels);
            assert_oracle_near_plane_cannot_clip(&uniform);
            let reference = render_depth(&device, &queue, &oracle, &resources, &draw, &uniform, 1.0)?;
            let control = render_depth(&device, &queue, &control, &resources, &draw, &uniform, 1.0)?;
            let actual = render_depth(&device, &queue, &resources.extrusion_pipeline, &resources, &draw, &uniform, 0.0)?;
            assert_visible_faces(&reference);
            let oracle_errors = color_mismatches(&control, &reference);
            let errors = color_mismatches(&actual, &reference);
            eprintln!("zoom={zoom}, cell={cell_pixels}, offset={offset}: production={errors}, oracle-control={oracle_errors}");
            assert!(
                oracle_errors <= 8,
                "Oracle near distances disagree: {oracle_errors} pixels at zoom={zoom}, offset={offset}"
            );
            // Only allow the same small intersection/tie budget as the two
            // high-precision references, not a percentage of wrong face winners.
            assert!(
                errors <= 8,
                "Wrong globe face winners: {errors}/4096 pixels at zoom={zoom}, cell={cell_pixels}, offset={offset}"
            );
        }
    }
    Ok(())
}
