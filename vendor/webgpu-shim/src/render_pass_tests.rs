//! Isolated GPU tests: no native bridge build, window, runtime, or test devdeps.

use std::future::{Future, IntoFuture};
use std::pin::pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use super::*;
use crate::*;

const SIZE: u32 = 8;
const NATIVE_DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth24PlusStencil8;
const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const BLACK: [u8; 4] = [0, 0, 0, 255];

struct ThreadWake(std::thread::Thread);

impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: IntoFuture>(future: F) -> F::Output {
    let mut future = pin!(future.into_future());
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
}

fn gpu() -> &'static Gpu {
    static GPU: OnceLock<Gpu> = OnceLock::new();
    GPU.get_or_init(|| {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter =
            block_on(instance.request_adapter(&Default::default())).expect("GPU tests require a native graphics adapter (software Vulkan is sufficient)");
        let (device, queue) = block_on(adapter.request_device(&Default::default())).unwrap();
        Gpu { device, queue }
    })
}

fn texture(format: wgpu::TextureFormat, samples: u32) -> wgpu::Texture {
    texture_with_extent(
        format,
        samples,
        wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        1,
    )
}

fn texture_with_extent(format: wgpu::TextureFormat, samples: u32, size: wgpu::Extent3d, mips: u32) -> wgpu::Texture {
    gpu().device.create_texture(&wgpu::TextureDescriptor {
        label: Some("Scoped private depth test attachment"),
        size,
        mip_level_count: mips,
        sample_count: samples,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | if samples == 1 {
                wgpu::TextureUsages::COPY_SRC
            } else {
                wgpu::TextureUsages::empty()
            },
        view_formats: &[],
    })
}

fn clear_depth(encoder: &mut wgpu::CommandEncoder, texture: &wgpu::Texture, value: f32) {
    let view = texture.create_view(&Default::default());
    drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(value),
                store: wgpu::StoreOp::Store,
            }),
            stencil_ops: texture.format().has_stencil_aspect().then_some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(7),
                store: wgpu::StoreOp::Store,
            }),
        }),
        ..Default::default()
    }));
}

#[derive(Clone, Copy)]
struct NativeConfig {
    samples: u32,
    resolve: bool,
    stores: [WGPUStoreOp; 3],
    read_only: bool,
    depth_attachment: bool,
    colors: usize,
}

impl Default for NativeConfig {
    fn default() -> Self {
        Self {
            samples: 1,
            resolve: false,
            stores: [WGPUStoreOp_Store; 3],
            read_only: false,
            depth_attachment: true,
            colors: 1,
        }
    }
}

struct Scene {
    output: wgpu::Texture,
    private: wgpu::TextureView,
    parent: WGPUCommandEncoder,
    pass: WGPURenderPassEncoder,
}

impl Scene {
    fn new(config: NativeConfig) -> Self {
        let output = texture(wgpu::TextureFormat::Rgba8Unorm, 1);
        let color = texture(wgpu::TextureFormat::Rgba8Unorm, config.samples);
        let extra = texture(wgpu::TextureFormat::Rgba8Unorm, config.samples);
        let native_depth = texture(NATIVE_DEPTH, config.samples);
        let private_texture = texture(wgpu::TextureFormat::Depth32Float, 1);
        let mut encoder = gpu().device.create_command_encoder(&Default::default());
        clear_depth(&mut encoder, &private_texture, 1.0);
        clear_depth(&mut encoder, &native_depth, 0.25);
        let parent = WGPUCommandEncoderImpl(Mutex::new(Some(encoder))).to_pointer();
        let color_texture = if config.samples == 1 { &output } else { &color };
        let views: Vec<_> = [color_texture, &extra, &native_depth, &output]
            .iter()
            .map(|texture| WGPUTextureViewImpl(texture.create_view(&Default::default())).to_pointer())
            .collect();
        let colors: Vec<_> = (0..config.colors)
            .map(|index| WGPURenderPassColorAttachment {
                nextInChain: std::ptr::null_mut(),
                view: views[index],
                depthSlice: u32::MAX,
                resolveTarget: if config.resolve { views[3] } else { std::ptr::null() },
                loadOp: WGPULoadOp_Clear,
                storeOp: config.stores[0],
                clearValue: WGPUColor {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                },
            })
            .collect();
        let depth = WGPURenderPassDepthStencilAttachment {
            nextInChain: std::ptr::null_mut(),
            view: views[2],
            depthLoadOp: WGPULoadOp_Clear,
            depthStoreOp: config.stores[1],
            depthClearValue: 0.9,
            depthReadOnly: u32::from(config.read_only),
            stencilLoadOp: WGPULoadOp_Clear,
            stencilStoreOp: config.stores[2],
            stencilClearValue: 3,
            stencilReadOnly: u32::from(config.read_only),
        };
        let pass = begin_native(parent, &colors, config.depth_attachment.then_some(&depth));
        // Drop all native view owners and textures: the pass snapshot must own clones.
        for view in views {
            unsafe { wgpuTextureViewRelease(view) };
        }
        Self {
            output,
            private: private_texture.create_view(&Default::default()),
            parent,
            pass,
        }
    }

    fn target_size(&self) -> Option<[u32; 2]> {
        unsafe { private_depth_target_size(self.pass as usize) }
    }

    fn scope(&self, draw: impl FnOnce(&mut wgpu::RenderPass<'_>)) -> bool {
        unsafe { with_render_pass_with_depth(self.pass as usize, &self.private, draw) }
    }

    fn draw(&self, pipeline: &wgpu::RenderPipeline, rect: [u32; 4]) {
        unsafe {
            with_render_pass(self.pass as usize, |pass| {
                pass.set_pipeline(pipeline);
                pass.set_stencil_reference(7);
                pass.set_scissor_rect(rect[0], rect[1], rect[2], rect[3]);
                pass.draw(0..3, 0..1);
            });
        }
    }

    fn pixels(&self) -> Vec<[u8; 4]> {
        unsafe { wgpuRenderPassEncoderEnd(self.pass) };
        let size = self.output.size();
        let buffer = gpu().device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Scoped pass color readback"),
            size: u64::from(size.height * 256),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut parent = unsafe { &*self.parent }.0.lock().unwrap();
        parent.as_mut().unwrap().copy_texture_to_buffer(
            self.output.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: None,
                },
            },
            self.output.size(),
        );
        drop(parent);
        let command = unsafe { wgpuCommandEncoderFinish(self.parent, std::ptr::null()) };
        let command_buffer = unsafe { &*command }.0.lock().unwrap().take().unwrap();
        let submission = gpu().queue.submit([command_buffer]);
        unsafe { wgpuCommandBufferRelease(command) };
        let (send, receive) = std::sync::mpsc::channel();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| send.send(result).unwrap());
        gpu()
            .device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(Duration::from_secs(30)),
            })
            .unwrap();
        receive.recv_timeout(Duration::from_secs(30)).unwrap().unwrap();
        let bytes = buffer.slice(..).get_mapped_range();
        bytes
            .chunks_exact(256)
            .flat_map(|row| row[..(size.width * 4) as usize].chunks_exact(4).map(|pixel| pixel.try_into().unwrap()))
            .collect()
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        unsafe {
            wgpuRenderPassEncoderEnd(self.pass);
            wgpuRenderPassEncoderRelease(self.pass);
            wgpuCommandEncoderRelease(self.parent);
        }
    }
}

fn begin_native(
    parent: WGPUCommandEncoder,
    colors: &[WGPURenderPassColorAttachment],
    depth: Option<&WGPURenderPassDepthStencilAttachment>,
) -> WGPURenderPassEncoder {
    let descriptor = WGPURenderPassDescriptor {
        nextInChain: std::ptr::null_mut(),
        label: WGPUStringView {
            data: std::ptr::null(),
            length: 0,
        },
        colorAttachmentCount: colors.len(),
        colorAttachments: colors.as_ptr(),
        depthStencilAttachment: depth.map_or(std::ptr::null(), std::ptr::from_ref),
        occlusionQuerySet: std::ptr::null(),
        timestampWrites: std::ptr::null(),
    };
    unsafe { wgpuCommandEncoderBeginRenderPass(parent, &descriptor) }
}

fn depth_state(format: wgpu::TextureFormat, compare: wgpu::CompareFunction) -> wgpu::DepthStencilState {
    wgpu::DepthStencilState {
        format,
        depth_write_enabled: Some(true),
        depth_compare: Some(compare),
        stencil: Default::default(),
        bias: Default::default(),
    }
}

fn stencil(compare: wgpu::CompareFunction, pass_op: wgpu::StencilOperation) -> wgpu::StencilState {
    let face = wgpu::StencilFaceState {
        compare,
        pass_op,
        ..Default::default()
    };
    wgpu::StencilState {
        front: face,
        back: face,
        read_mask: 0xff,
        write_mask: 0xff,
    }
}

fn pipeline(color: [u8; 4], z: f32, depth: wgpu::DepthStencilState) -> wgpu::RenderPipeline {
    let [r, g, b, a] = color.map(|channel| f32::from(channel) / 255.0);
    let shader = gpu().device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("Scoped private depth test shader"),
        source: wgpu::ShaderSource::Wgsl(
            format!(
                r#"
            @vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4f {{
                let p = array(vec2f(-1.0, -1.0), vec2f(3.0, -1.0), vec2f(-1.0, 3.0));
                return vec4f(p[i], {z}, 1.0);
            }}
            @fragment fn fs() -> @location(0) vec4f {{ return vec4f({r}, {g}, {b}, {a}); }}
        "#
            )
            .into(),
        ),
    });
    gpu().device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("Scoped private depth test pipeline"),
        layout: None,
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: Default::default(),
        depth_stencil: Some(depth),
        multisample: Default::default(),
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba8Unorm,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn assert_pixels(pixels: Vec<[u8; 4]>, expected: impl Fn(u32, u32) -> [u8; 4]) {
    for (index, actual) in pixels.into_iter().enumerate() {
        let (x, y) = (index as u32 % SIZE, index as u32 / SIZE);
        assert_eq!(actual, expected(x, y), "pixel ({x}, {y})");
    }
}

#[test]
fn private_scopes_preserve_predraw_color_and_clear_depth_to_zero_every_time() {
    let scene = Scene::new(Default::default());
    let red = pipeline(RED, 0.35, depth_state(NATIVE_DEPTH, wgpu::CompareFunction::Always));
    let green = pipeline(GREEN, 0.2, depth_state(wgpu::TextureFormat::Depth32Float, wgpu::CompareFunction::Greater));
    let blue = pipeline(BLUE, 0.1, depth_state(wgpu::TextureFormat::Depth32Float, wgpu::CompareFunction::Greater));
    let zero = pipeline(BLACK, 0.0, depth_state(wgpu::TextureFormat::Depth32Float, wgpu::CompareFunction::Greater));
    scene.draw(&red, [0, 0, SIZE, SIZE]);
    assert!(scene.scope(|pass| {
        pass.set_pipeline(&zero);
        pass.draw(0..3, 0..1); // Greater at exactly zero must fail after the clear.
        pass.set_scissor_rect(4, 0, 4, SIZE);
        pass.set_pipeline(&green);
        pass.draw(0..3, 0..1);
    }));
    assert!(scene.scope(|pass| {
        pass.set_scissor_rect(6, 0, 2, SIZE);
        pass.set_pipeline(&blue);
        pass.draw(0..3, 0..1); // Lower than the previous scope, still above its fresh zero.
    }));
    assert_pixels(scene.pixels(), |x, _| {
        if x < 4 {
            RED
        } else if x < 6 {
            GREEN
        } else {
            BLUE
        }
    });
}

#[test]
fn continuation_loads_original_depth_and_stencil_after_private_draw() {
    let scene = Scene::new(Default::default());
    let mut native = depth_state(NATIVE_DEPTH, wgpu::CompareFunction::Always);
    native.stencil = stencil(wgpu::CompareFunction::Always, wgpu::StencilOperation::Replace);
    let red = pipeline(RED, 0.35, native);
    let green = pipeline(GREEN, 0.8, depth_state(wgpu::TextureFormat::Depth32Float, wgpu::CompareFunction::Greater));
    let mut continuation = depth_state(NATIVE_DEPTH, wgpu::CompareFunction::Equal);
    continuation.stencil = stencil(wgpu::CompareFunction::Equal, wgpu::StencilOperation::Keep);
    let blue = pipeline(BLUE, 0.35, continuation);
    scene.draw(&red, [0, 0, SIZE, SIZE]);
    assert!(scene.scope(|pass| {
        pass.set_pipeline(&green);
        pass.set_scissor_rect(4, 0, 4, SIZE);
        pass.draw(0..3, 0..1);
    }));
    scene.draw(&blue, [0, 4, SIZE, 4]);
    assert_pixels(scene.pixels(), |x, y| {
        if y >= 4 {
            BLUE
        } else if x >= 4 {
            GREEN
        } else {
            RED
        }
    });
}

#[test]
fn private_pass_inherits_cabi_viewport_depth_range_and_scissor() {
    let scene = Scene::new(Default::default());
    let green = pipeline(GREEN, 0.0, depth_state(wgpu::TextureFormat::Depth32Float, wgpu::CompareFunction::Greater));
    unsafe {
        wgpuRenderPassEncoderSetViewport(scene.pass, 2.0, 1.0, 4.0, 6.0, 0.25, 0.75);
        wgpuRenderPassEncoderSetScissorRect(scene.pass, 3, 2, 2, 3);
    }
    assert!(scene.scope(|pass| {
        pass.set_pipeline(&green);
        pass.draw(0..3, 0..1);
    }));
    assert_pixels(scene.pixels(), |x, y| if (3..5).contains(&x) && (2..5).contains(&y) { GREEN } else { BLACK });
}

#[test]
fn private_depth_also_supports_a_native_color_only_pass() {
    let scene = Scene::new(NativeConfig {
        depth_attachment: false,
        ..Default::default()
    });
    let green = pipeline(GREEN, 0.2, depth_state(wgpu::TextureFormat::Depth32Float, wgpu::CompareFunction::Greater));
    assert_eq!(scene.target_size(), Some([SIZE, SIZE]));
    assert!(scene.scope(|pass| {
        pass.set_pipeline(&green);
        pass.draw(0..3, 0..1);
    }));
    assert_pixels(scene.pixels(), |_, _| GREEN);
}

#[test]
fn cabi_viewport_depth_range_and_scissor_survive_repeated_switches() {
    let scene = Scene::new(Default::default());
    let blue = pipeline(BLUE, 0.0, depth_state(NATIVE_DEPTH, wgpu::CompareFunction::Equal));
    // Native depth starts at 0.9; write 0.25, which a restored viewport maps z=0 to.
    let red = pipeline(RED, 0.25, depth_state(NATIVE_DEPTH, wgpu::CompareFunction::Always));
    scene.draw(&red, [0, 0, SIZE, SIZE]);
    unsafe {
        wgpuRenderPassEncoderSetViewport(scene.pass, 2.0, 1.0, 4.0, 6.0, 0.25, 0.75);
        wgpuRenderPassEncoderSetScissorRect(scene.pass, 3, 2, 2, 3);
    }
    let green = pipeline(GREEN, 0.0, depth_state(wgpu::TextureFormat::Depth32Float, wgpu::CompareFunction::Greater));
    assert!(scene.scope(|pass| {
        pass.set_pipeline(&green);
        pass.draw(0..3, 0..1);
    }));
    for _ in 0..2 {
        assert!(scene.scope(|pass| {
            pass.set_viewport(0.0, 0.0, SIZE as f32, SIZE as f32, 0.0, 1.0);
            pass.set_scissor_rect(0, 0, SIZE, SIZE);
        }));
    }
    unsafe {
        with_render_pass(scene.pass as usize, |pass| {
            pass.set_pipeline(&blue);
            pass.draw(0..3, 0..1);
        });
    }
    assert_pixels(scene.pixels(), |x, y| if (3..5).contains(&x) && (2..5).contains(&y) { BLUE } else { RED });
}

#[test]
fn callback_panic_restores_same_handle_and_unpoisoned_locks() {
    let scene = Scene::new(Default::default());
    let red = pipeline(RED, 0.35, depth_state(NATIVE_DEPTH, wgpu::CompareFunction::Always));
    let green = pipeline(GREEN, 0.8, depth_state(wgpu::TextureFormat::Depth32Float, wgpu::CompareFunction::Greater));
    let blue = pipeline(BLUE, 0.35, depth_state(NATIVE_DEPTH, wgpu::CompareFunction::Equal));
    scene.draw(&red, [0, 0, SIZE, SIZE]);
    let panic = catch_unwind(AssertUnwindSafe(|| {
        scene.scope(|pass| {
            pass.set_pipeline(&green);
            pass.set_scissor_rect(4, 0, 4, SIZE);
            pass.draw(0..3, 0..1);
            panic!("intentional scoped callback panic");
        })
    }));
    assert!(panic.is_err());
    assert_eq!(scene.target_size(), Some([SIZE, SIZE]));
    assert!(scene.scope(|_| {}), "panic must not poison either handle mutex");
    scene.draw(&blue, [0, 4, SIZE, 4]);
    assert_pixels(scene.pixels(), |x, y| {
        if y >= 4 {
            BLUE
        } else if x >= 4 {
            GREEN
        } else {
            RED
        }
    });
}

#[test]
fn read_only_depth_stencil_remain_read_only_and_preserved() {
    let scene = Scene::new(NativeConfig {
        read_only: true,
        stores: [WGPUStoreOp_Store, WGPUStoreOp_Discard, WGPUStoreOp_Discard],
        ..Default::default()
    });
    let mut native = depth_state(NATIVE_DEPTH, wgpu::CompareFunction::Equal);
    native.depth_write_enabled = Some(false);
    native.stencil = stencil(wgpu::CompareFunction::Equal, wgpu::StencilOperation::Keep);
    native.stencil.write_mask = 0;
    let red = pipeline(RED, 0.25, native.clone());
    let blue = pipeline(BLUE, 0.25, native);
    scene.draw(&red, [0, 0, SIZE, SIZE]);
    assert_eq!(scene.target_size(), Some([SIZE, SIZE]));
    assert!(scene.scope(|_| {}));
    let snapshot = unsafe { &*scene.pass }.1.attachments.depth_stencil.as_ref().unwrap();
    assert!(snapshot.depth.is_none() && snapshot.stencil.is_none());
    scene.draw(&blue, [0, 4, SIZE, 4]);
    assert_pixels(scene.pixels(), |_, y| if y >= 4 { BLUE } else { RED });
}

#[test]
fn discard_attachments_reject_before_ending_or_invalidating_native_state() {
    for aspect in 0..3 {
        let mut config = NativeConfig::default();
        config.stores[aspect] = WGPUStoreOp_Discard;
        let scene = Scene::new(config);
        let red = pipeline(RED, 0.35, depth_state(NATIVE_DEPTH, wgpu::CompareFunction::Always));
        unsafe { with_render_pass(scene.pass as usize, |pass| pass.set_pipeline(&red)) };
        assert_eq!(scene.target_size(), None);
        assert!(!scene.scope(|_| panic!("unsupported callback must not run")));
        // No rebind: a restart would lose the pipeline and fail GPU validation.
        unsafe { with_render_pass(scene.pass as usize, |pass| pass.draw(0..3, 0..1)) };
        if aspect != 0 {
            assert_pixels(scene.pixels(), |_, _| RED);
        }
    }
}

#[test]
fn multisample_resolve_and_multiple_or_missing_colors_are_unsupported() {
    let configs = [
        NativeConfig {
            samples: 4,
            ..Default::default()
        },
        NativeConfig {
            samples: 4,
            resolve: true,
            ..Default::default()
        },
        NativeConfig {
            colors: 2,
            ..Default::default()
        },
        NativeConfig {
            colors: 0,
            ..Default::default()
        },
    ];
    for config in configs {
        let scene = Scene::new(config);
        assert_eq!(scene.target_size(), None);
        assert!(!scene.scope(|_| panic!("unsupported callback must not run")));
        assert!(unsafe { &*scene.pass }.0.lock().unwrap().is_some());
    }
}

#[test]
fn wrong_private_format_and_ended_native_pass_do_not_call_callback() {
    let scene = Scene::new(Default::default());
    let wrong = texture(NATIVE_DEPTH, 1).create_view(&Default::default());
    assert!(!unsafe { with_render_pass_with_depth(scene.pass as usize, &wrong, |_| panic!("wrong format")) });
    assert!(scene.scope(|_| {}));
    unsafe { wgpuRenderPassEncoderEnd(scene.pass) };
    assert_eq!(scene.target_size(), None);
    assert!(!scene.scope(|_| panic!("ended native pass")));
}

#[test]
fn retained_parent_encoder_outlives_native_owner() {
    let mut scene = Scene::new(Default::default());
    unsafe { wgpuCommandEncoderRelease(scene.parent) };
    assert_eq!(scene.target_size(), Some([SIZE, SIZE]));
    assert!(scene.scope(|_| {}));
    // Reacquire a native owner solely for the test's normal finish/release path.
    let parent = Arc::clone(&unsafe { &*scene.pass }.1.parent);
    scene.parent = Arc::into_raw(parent);
    let red = pipeline(RED, 0.35, depth_state(NATIVE_DEPTH, wgpu::CompareFunction::Always));
    scene.draw(&red, [0, 0, SIZE, SIZE]);
    assert_pixels(scene.pixels(), |_, _| RED);
}

fn color_only_scene(size: wgpu::Extent3d, mips: u32) -> Scene {
    let output = texture_with_extent(wgpu::TextureFormat::Rgba8Unorm, 1, size, mips);
    let view = WGPUTextureViewImpl(output.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::D2),
        mip_level_count: Some(1),
        array_layer_count: Some(1),
        ..Default::default()
    }))
    .to_pointer();
    let color = WGPURenderPassColorAttachment {
        nextInChain: std::ptr::null_mut(),
        view,
        depthSlice: u32::MAX,
        resolveTarget: std::ptr::null(),
        loadOp: WGPULoadOp_Clear,
        storeOp: WGPUStoreOp_Store,
        clearValue: WGPUColor {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        },
    };
    let parent = WGPUCommandEncoderImpl(Mutex::new(Some(gpu().device.create_command_encoder(&Default::default())))).to_pointer();
    let pass = begin_native(parent, &[color], None);
    unsafe { wgpuTextureViewRelease(view) };
    let private = texture_with_extent(
        wgpu::TextureFormat::Depth32Float,
        1,
        wgpu::Extent3d {
            depth_or_array_layers: 1,
            ..size
        },
        1,
    )
    .create_view(&Default::default());
    Scene { output, private, parent, pass }
}

#[test]
fn target_size_reports_actual_non_square_color_extent_before_and_after_scopes() {
    let size = wgpu::Extent3d {
        width: 7,
        height: 5,
        depth_or_array_layers: 1,
    };
    let scene = color_only_scene(size, 1);
    assert_eq!(scene.target_size(), Some([7, 5]));
    let green = pipeline(GREEN, 0.2, depth_state(wgpu::TextureFormat::Depth32Float, wgpu::CompareFunction::Greater));
    assert!(scene.scope(|pass| {
        pass.set_pipeline(&green);
        pass.draw(0..3, 0..1);
    }));
    assert_eq!(scene.target_size(), Some([7, 5]));
    assert_eq!(scene.pixels(), vec![GREEN; 35]);
    assert_eq!(scene.target_size(), None);
    assert!(!scene.scope(|_| panic!("ended native pass")));
}

#[test]
fn target_size_and_scope_reject_mipmapped_or_layered_color_textures() {
    for (mips, layers) in [(2, 1), (1, 2)] {
        let scene = color_only_scene(
            wgpu::Extent3d {
                width: SIZE,
                height: SIZE,
                depth_or_array_layers: layers,
            },
            mips,
        );
        // Even a view selecting just one mip/layer cannot expose its subresources.
        assert_eq!(scene.target_size(), None);
        assert!(!scene.scope(|_| panic!("unsupported callback must not run")));
        assert!(unsafe { &*scene.pass }.0.lock().unwrap().is_some());
    }
}

#[test]
fn mismatched_private_depth_size_preserves_native_pipeline_and_scissor() {
    let scene = Scene::new(Default::default());
    let red = pipeline(RED, 0.35, depth_state(NATIVE_DEPTH, wgpu::CompareFunction::Always));
    unsafe {
        with_render_pass(scene.pass as usize, |pass| pass.set_pipeline(&red));
        wgpuRenderPassEncoderSetScissorRect(scene.pass, 2, 2, 4, 4);
    }
    for [width, height] in [[7, 8], [8, 7], [9, 8], [8, 9]] {
        assert_eq!(scene.target_size(), Some([SIZE, SIZE]));
        let depth = texture_with_extent(
            wgpu::TextureFormat::Depth32Float,
            1,
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            1,
        )
        .create_view(&Default::default());
        assert!(!unsafe { with_render_pass_with_depth(scene.pass as usize, &depth, |_| panic!("mismatched depth callback must not run")) });
    }
    // No rebind: ending/restarting the native pass would lose this pipeline.
    unsafe { with_render_pass(scene.pass as usize, |pass| pass.draw(0..3, 0..1)) };
    assert_pixels(scene.pixels(), |x, y| if (2..6).contains(&x) && (2..6).contains(&y) { RED } else { BLACK });
}

#[test]
fn target_size_and_scope_require_a_usable_parent_encoder() {
    let scene = Scene::new(Default::default());
    let parent = &unsafe { &*scene.pass }.1.parent;
    // Moving the forgotten-lifetime encoder is legal; do not finish/drop it.
    let encoder = parent.0.lock().unwrap().take().unwrap();
    let size = scene.target_size();
    let supported = scene.scope(|_| panic!("missing parent callback must not run"));
    *parent.0.lock().unwrap() = Some(encoder);
    assert_eq!(size, None);
    assert!(!supported);
    assert_eq!(scene.target_size(), Some([SIZE, SIZE]));
    assert!(scene.scope(|_| {}));
}

fn assert_poisoned_lock_rejects_query_and_scope<T>(scene: &Scene, mutex: &Mutex<T>) {
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            let _guard = mutex.lock().unwrap();
            panic!("intentional capability lock poison");
        }))
        .is_err()
    );
    let result = (scene.target_size(), scene.scope(|_| panic!("poisoned handle callback must not run")));
    // Repair before asserting, so normal C-ABI teardown remains usable on failure.
    mutex.clear_poison();
    assert_eq!(result, (None, false));
}

#[test]
fn target_size_and_scope_reject_the_same_poisoned_locks() {
    let scene = Scene::new(Default::default());
    let handle = unsafe { &*scene.pass };
    assert_poisoned_lock_rejects_query_and_scope(&scene, &handle.0);
    assert_poisoned_lock_rejects_query_and_scope(&scene, &handle.1.raster);
    assert_poisoned_lock_rejects_query_and_scope(&scene, &handle.1.parent.0);
    assert_eq!(scene.target_size(), Some([SIZE, SIZE]));
    assert!(scene.scope(|_| {}));
}
