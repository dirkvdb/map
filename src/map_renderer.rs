use std::cell::Cell;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use gpui_kit::RenderImage;
#[cfg(test)]
use image::RgbaImage;
use maplibre_native::{Continuous, ImageRenderer, ImageRendererBuilder, RunLoopHandle, Size};

use crate::map_camera::MapCamera;
use crate::map_readback::GpuReadback;

pub(crate) const STYLE_URL: &str = "https://demotiles.maplibre.org/style.json";

#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Viewport {
    pub(crate) width: NonZeroU32,
    pub(crate) height: NonZeroU32,
    pub(crate) scale: f32,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) struct MapRequest {
    pub(crate) viewport: Viewport,
    pub(crate) camera: MapCamera,
}

pub(crate) struct MapFrame {
    pub(crate) viewport: Viewport,
    pub(crate) image: Result<Arc<RenderImage>>,
}

pub(crate) struct MapRenderer {
    sender: mpsc::Sender<MapRequest>,
    frames: async_channel::Receiver<MapFrame>,
}

impl MapRenderer {
    pub(crate) fn new() -> Result<Self> {
        let (sender, receiver) = mpsc::channel::<MapRequest>();
        let (frame_sender, frames) = async_channel::bounded(1);
        let pending_frame = frames.clone();
        // The !Send renderer and its run loop remain on this dedicated thread.
        // Dropping the view closes the channel; the worker finishes its bounded
        // in-flight request, drops all native resources, and exits.
        std::thread::Builder::new()
            .name("map-renderer".into())
            .spawn(move || {
                let mut map: Option<NativeMap> = None;
                let mut current: Option<MapRequest> = None;
                let mut next_frame = Instant::now();
                loop {
                    let incoming = if map.is_none() {
                        receiver.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                    } else {
                        receiver.recv_timeout(next_frame.saturating_duration_since(Instant::now()))
                    };
                    let mut request = match incoming {
                        Ok(request) => Some(request),
                        Err(mpsc::RecvTimeoutError::Timeout) => None,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    for latest in receiver.try_iter() {
                        request = Some(latest);
                    }
                    if let Some(request) = request {
                        current = Some(request);
                    }
                    let Some(current) = current else { continue };
                    let frame_started = Instant::now();
                    let result = (|| {
                        if map.as_ref().is_none_or(|map| map.viewport.scale != current.viewport.scale) {
                            map = Some(NativeMap::new(current.viewport)?);
                        }
                        let map = map.as_mut().context("Map renderer unavailable")?;
                        if request.is_some() {
                            map.apply_request(current);
                        }
                        if Instant::now() < next_frame {
                            return Ok(None);
                        }
                        map.poll_frame()?
                            .map(|texture| {
                                let pixels = map.readback.read(&texture, &map.device, &map.queue)?;
                                Ok(Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])))
                            })
                            .transpose()
                    })();
                    if result.is_err() {
                        map = None;
                    }
                    if let Ok(None) = result {
                        // Poll native resource/fade callbacks without busy-waiting.
                        if Instant::now() >= next_frame {
                            next_frame = Instant::now() + Duration::from_millis(16);
                        }
                        continue;
                    }
                    let frame = MapFrame {
                        viewport: current.viewport,
                        image: result.and_then(|image| image.context("Missing map frame")),
                    };
                    if let Err(async_channel::TrySendError::Full(frame)) = frame_sender.try_send(frame) {
                        // Keep one newest completed frame if GPUI is still busy.
                        let _ = pending_frame.try_recv();
                        let _ = frame_sender.try_send(frame);
                    }
                    next_frame = frame_started + Duration::from_millis(16);
                }
            })
            .context("Could not start the map rendering thread")?;
        Ok(Self { sender, frames })
    }

    pub(crate) fn render(&self, request: MapRequest) -> Result<()> {
        self.sender.send(request).context("Map rendering thread stopped")?;
        Ok(())
    }

    pub(crate) fn frames(&self) -> async_channel::Receiver<MapFrame> {
        self.frames.clone()
    }
}

struct NativeMap {
    renderer: ImageRenderer<Continuous>,
    viewport: Viewport,
    device: wgpu::Device,
    queue: wgpu::Queue,
    idle: Rc<Cell<bool>>,
    needs_frame: Rc<Cell<bool>>,
    readback: GpuReadback,
    deadline: Instant,
}

impl NativeMap {
    fn new(viewport: Viewport) -> Result<Self> {
        // The wgpu FFI backend requires a host-supplied device and queue. This
        // initialization can later accept GPUI's device instead of creating one.
        let (device, queue) = futures_lite::future::block_on(async {
            let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions::default())
                .await
                .context("Could not find a wgpu adapter for the map")?;
            adapter
                .request_device(&wgpu::DeviceDescriptor {
                    label: Some("MapLibre device"),
                    ..Default::default()
                })
                .await
                .context("Could not create the MapLibre wgpu device")
        })?;
        let mut renderer = ImageRendererBuilder::new()
            .with_size(viewport.width, viewport.height)
            .with_pixel_ratio(viewport.scale)
            .build_continuous_renderer();
        renderer.set_device_queue(device.clone(), queue.clone());
        let idle = Rc::new(Cell::new(false));
        renderer.map_observer().set_did_become_idle_callback({
            let idle = idle.clone();
            move || idle.set(true)
        });
        let needs_frame = Rc::new(Cell::new(true));
        renderer.set_render_requested_callback({
            let needs_frame = needs_frame.clone();
            move || needs_frame.set(true)
        });
        let run_loop = RunLoopHandle::current();
        let style = renderer.load_style_from_url(&STYLE_URL.parse()?);
        wait_until_ready(&run_loop, Instant::now() + Duration::from_secs(30), || style.is_ready())?;
        style.finish().context("Could not load the vector map style")?;
        Ok(Self {
            renderer,
            viewport,
            readback: GpuReadback::new(&device),
            device,
            queue,
            idle,
            needs_frame,
            deadline: Instant::now() + Duration::from_secs(30),
        })
    }

    #[cfg(test)]
    fn render(&mut self, request: MapRequest) -> Result<RgbaImage> {
        let texture = self.render_texture(request)?;
        self.readback.read(&texture, &self.device, &self.queue)
    }

    #[cfg(test)]
    fn render_texture(&mut self, request: MapRequest) -> Result<wgpu::Texture> {
        self.apply_request(request);
        let mut latest = None;
        while !self.idle.get() {
            if let Some(texture) = self.poll_frame()? {
                latest = Some(texture)
            }
            if !self.idle.get() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        latest.context("MapLibre did not produce a wgpu texture")
    }

    fn apply_request(&mut self, request: MapRequest) {
        self.idle.set(false);
        if self.viewport != request.viewport {
            self.renderer.set_map_size(Size {
                width: request.viewport.width.get(),
                height: request.viewport.height.get(),
            });
            self.viewport = request.viewport;
        }
        self.renderer.update_camera(&request.camera.update());
        self.needs_frame.set(true);
        self.deadline = Instant::now() + Duration::from_secs(30);
    }

    fn poll_frame(&mut self) -> Result<Option<wgpu::Texture>> {
        ensure!(self.idle.get() || Instant::now() < self.deadline, "Map rendering timed out");
        RunLoopHandle::current().tick();
        if self.needs_frame.replace(false) {
            self.renderer.render_once();
            return self
                .renderer
                .take_texture()
                .map(Some)
                .context("MapLibre did not produce a wgpu texture");
        }
        Ok(None)
    }
}

fn wait_until_ready(run_loop: &RunLoopHandle, deadline: Instant, is_ready: impl Fn() -> bool) -> Result<()> {
    while !is_ready() {
        ensure!(Instant::now() < deadline, "Map loading timed out. Check your connection and retry.");
        run_loop.tick();
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "manual GPU performance measurement; requires internet access"]
    fn navigation_frame_timings() -> Result<()> {
        let _ = env_logger::builder().is_test(true).try_init();

        let viewport = Viewport {
            width: NonZeroU32::new(1200).unwrap(),
            height: NonZeroU32::new(760).unwrap(),
            scale: 1.5,
        };
        let mut camera = MapCamera::default();
        camera.zoom_at(1.5, (0.0, 0.0));
        let mut map = NativeMap::new(viewport)?;
        map.render(MapRequest { viewport, camera })?;
        let mut rendering = Duration::ZERO;
        let mut readback = Duration::ZERO;
        for _ in 0..12 {
            camera.pan(10.0, 5.0);
            camera.zoom_at(0.01, (0.0, 0.0));
            let start = Instant::now();
            map.apply_request(MapRequest { viewport, camera });
            let texture = loop {
                if let Some(texture) = map.poll_frame()? {
                    break texture;
                }
            };
            let rendered = Instant::now();
            let image = map.readback.read(&texture, &map.device, &map.queue)?;
            rendering += rendered - start;
            readback += rendered.elapsed();
            assert_eq!(image.dimensions(), (1800, 1140));
        }
        log::info!(
            "Average navigation frame: first-frame rendering={:?}, readback/copy={:?}",
            rendering / 12,
            readback / 12
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires a native graphics device and internet access"]
    fn streaming_worker_delivers_frames_and_accepts_new_viewports() -> Result<()> {
        let renderer = MapRenderer::new()?;
        let frames = renderer.frames();
        let mut viewport = Viewport {
            width: NonZeroU32::new(800).unwrap(),
            height: NonZeroU32::new(600).unwrap(),
            scale: 1.0,
        };
        renderer.render(MapRequest {
            viewport,
            camera: MapCamera::default(),
        })?;
        let wait_for_frame = |viewport: Viewport| -> Result<Arc<RenderImage>> {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                ensure!(Instant::now() < deadline, "Streaming map frame timed out");
                if let Ok(frame) = frames.try_recv() {
                    let image = frame.image?;
                    if frame.viewport == viewport {
                        let colors: std::collections::HashSet<_> = image.as_bytes(0).unwrap().chunks_exact(4).collect();
                        if colors.len() > 100 {
                            return Ok(image);
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let first = wait_for_frame(viewport)?;
        assert_eq!(first.size(0).width.0, 800);
        viewport.width = NonZeroU32::new(1000).unwrap();
        let mut camera = MapCamera::default();
        camera.zoom_at(1.0, (0.0, 0.0));
        renderer.render(MapRequest { viewport, camera })?;
        let resized = wait_for_frame(viewport)?;
        assert_eq!(resized.size(0).width.0, 1000);
        drop(renderer);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !frames.is_closed() {
            ensure!(Instant::now() < deadline, "Map worker did not shut down");
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires a wgpu-compatible graphics device and internet access to the demo vector tiles"]
    fn vector_basemap_renders_geographic_content() -> Result<()> {
        let viewport = Viewport {
            width: NonZeroU32::new(800).unwrap(),
            height: NonZeroU32::new(600).unwrap(),
            scale: 1.0,
        };
        let mut map = NativeMap::new(viewport)?;
        let image = map.render(MapRequest {
            viewport,
            camera: MapCamera::default(),
        })?;
        assert_eq!(image.dimensions(), (800, 600));
        let colors: std::collections::HashSet<_> = image.pixels().map(|pixel| pixel.0).collect();
        assert!(
            colors.len() > 100,
            "Expected vector features and labels, got {} colors",
            colors.len()
        );
        assert!(image.pixels().all(|pixel| pixel[3] == 255));
        Ok(())
    }

    #[test]
    #[ignore = "requires a native graphics device and internet access"]
    fn retained_renderer_updates_camera_and_viewport() -> Result<()> {
        let mut viewport = Viewport {
            width: NonZeroU32::new(800).unwrap(),
            height: NonZeroU32::new(600).unwrap(),
            scale: 1.0,
        };
        let mut camera = MapCamera::default();
        let mut map = NativeMap::new(viewport)?;
        let before = map.render(MapRequest { viewport, camera })?;
        camera.pan(150.0, 70.0);
        camera.zoom_at(1.0, (120.0, -80.0));
        let after = map.render(MapRequest { viewport, camera })?;
        assert!(before.as_raw() != after.as_raw(), "Camera changes must change the rendered image");
        viewport.width = NonZeroU32::new(1000).unwrap();
        let resized = map.render(MapRequest { viewport, camera })?;
        assert_eq!(resized.dimensions(), (1000, 600));
        Ok(())
    }
}
