use std::sync::Arc;

use gpui_kit::{App, Context, IntoElement, Render, RenderImage, Task, Window};

use super::extent_map::{Basemap, ExtentMap};
use crate::map_renderer::{BasemapStyle, MapFrame, MapRenderer};

/// Owns the one-shot basemap render behind an extent preview. The worker lives
/// only until its frame settles, fails, or the dialog closes.
pub(super) struct ExtentPreview {
    map: ExtentMap,
    basemap: Basemap,
    _render: Option<Task<()>>,
}

impl ExtentPreview {
    pub(super) fn new(map: ExtentMap, renderer: Option<MapRenderer>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.on_release_in(window, |preview, window, cx| {
            if let Basemap::Ready(image) = std::mem::take(&mut preview.basemap) {
                cx.drop_image(image, Some(window));
            }
        })
        .detach();
        let render = renderer
            .and_then(|renderer| Self::request_basemap(&map, renderer, window, cx))
            .map(|(renderer, frames)| Self::watch_frames(renderer, frames, cx));
        Self {
            map,
            basemap: if render.is_some() { Basemap::Loading } else { Basemap::Unavailable },
            _render: render,
        }
    }

    /// Queues the single render and hands back the worker with its frames, so the
    /// caller decides how they are pumped.
    fn request_basemap(map: &ExtentMap, renderer: MapRenderer, window: &Window, cx: &App) -> Option<(MapRenderer, async_channel::Receiver<MapFrame>)> {
        let request = map.basemap_request(window.scale_factor())?;
        let basemap = BasemapStyle::default_for_theme(cx);
        if let Err(error) = renderer.render_layer(request, None, 1.0, cx.reduce_motion(), basemap, false) {
            log::warn!("Could not queue the extent preview basemap ({basemap:?}): {error:#}");
            return None;
        }
        let frames = renderer.frames();
        Some((renderer, frames))
    }

    fn watch_frames(renderer: MapRenderer, frames: async_channel::Receiver<MapFrame>, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            // Dropping the renderer with this task joins its worker.
            let _renderer = renderer;
            while let Ok(frame) = frames.recv().await
                && this.update(cx, |preview, cx| preview.apply_frame(frame, cx)).is_ok_and(|finished| !finished)
            {}
        })
    }

    /// Progressive frames replace each other; a failure keeps any frame already shown.
    /// Returns whether the render is finished: settled, or failed.
    fn apply_frame(&mut self, frame: MapFrame, cx: &mut Context<Self>) -> bool {
        let finished = frame.settled || frame.image.is_err();
        match frame.image {
            Ok(image) => self.replace_image(image, cx),
            Err(error) => {
                log::warn!("Could not render the extent preview basemap ({:?}): {error:#}", frame.basemap);
                if !matches!(self.basemap, Basemap::Ready(_)) {
                    self.basemap = Basemap::Unavailable;
                }
            }
        }
        cx.notify();
        finished
    }

    fn replace_image(&mut self, image: Arc<RenderImage>, cx: &mut Context<Self>) {
        if let Basemap::Ready(previous) = std::mem::replace(&mut self.basemap, Basemap::Ready(image.clone()))
            && !Arc::ptr_eq(&previous, &image)
        {
            // Dropping the Arc does not evict GPUI's uploaded texture.
            cx.drop_image(previous, None);
        }
    }

    #[cfg(test)]
    fn image(&self) -> Option<&Arc<RenderImage>> {
        match &self.basemap {
            Basemap::Ready(image) => Some(image),
            Basemap::Loading | Basemap::Unavailable => None,
        }
    }
}

impl Render for ExtentPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.map.clone().with_basemap(self.basemap.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AppContext, Entity, TestAppContext, px, size};

    use super::*;
    use crate::map_renderer::Viewport;

    fn open_preview(renderer: Option<MapRenderer>, cx: &mut TestAppContext) -> (Entity<ExtentPreview>, gpui_kit::AnyWindowHandle) {
        cx.update(gpui_kit::init);
        let mut preview = None;
        let handle = cx.open_window(size(px(800.), px(600.)), |window, cx| {
            let entity = cx.new(|cx| ExtentPreview::new(ExtentMap::new([112., -44., 154., -10.]), renderer, window, cx));
            preview = Some(entity.clone());
            gpui_kit::base::Root::new(entity, window, cx)
        });
        (preview.unwrap(), handle.into())
    }

    fn frame(image: anyhow::Result<Arc<RenderImage>>, settled: bool) -> MapFrame {
        MapFrame {
            viewport: Viewport {
                width: 640.try_into().unwrap(),
                height: 320.try_into().unwrap(),
                scale: 1.,
            },
            basemap: BasemapStyle::Light,
            tile_boundaries: false,
            raster_layer_id: None,
            vector_layer_id: None,
            settled,
            playback: None,
            image,
        }
    }

    fn test_image() -> Arc<RenderImage> {
        Arc::new(RenderImage::new(vec![image::Frame::new(image::RgbaImage::new(4, 2))]))
    }

    #[gpui_kit::test]
    fn previews_without_a_renderer_keep_the_extent_and_report_the_missing_basemap(cx: &mut TestAppContext) {
        let (preview, handle) = open_preview(None, cx);
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("dataset-extent-map").visible());
            assert!(window.find("dataset-extent-basemap-unavailable").visible());
            assert!(window.try_find("dataset-extent-unavailable").is_none());
            assert!(preview.read(cx).image().is_none() && preview.read(cx)._render.is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn frames_replace_the_basemap_and_later_failures_keep_the_frame_shown(cx: &mut TestAppContext) {
        let (preview, handle) = open_preview(None, cx);
        let (first, second) = (test_image(), test_image());
        cx.update_window(handle, |_, window, cx| {
            assert!(!preview.update(cx, |preview, cx| preview.apply_frame(frame(Ok(first.clone()), false), cx)));
            window.render_frame(cx);
            assert!(window.try_find("dataset-extent-basemap-unavailable").is_none());
            assert!(preview.read(cx).image().is_some_and(|image| Arc::ptr_eq(image, &first)));
            assert!(preview.update(cx, |preview, cx| preview.apply_frame(frame(Ok(second.clone()), true), cx)));
            assert!(preview.read(cx).image().is_some_and(|image| Arc::ptr_eq(image, &second)));
            assert!(preview.update(cx, |preview, cx| preview.apply_frame(frame(Err(anyhow::anyhow!("offline")), false), cx)));
            window.render_frame(cx);
            assert!(preview.read(cx).image().is_some_and(|image| Arc::ptr_eq(image, &second)));
            assert!(window.try_find("dataset-extent-basemap-unavailable").is_none());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    #[ignore = "requires a wgpu-compatible graphics device; uses a local background-only style"]
    fn previews_render_the_default_basemap_from_their_own_worker(cx: &mut TestAppContext) {
        let renderer = MapRenderer::with_test_style(
            r##"{"version":8,"sources":{},"layers":[{"id":"background","type":"background","paint":{"background-color":"#f0f0f0"}}]}"##,
        )
        .unwrap();
        let (preview, handle) = open_preview(None, cx);
        // Pump the worker channel on the test thread; the scheduler rejects foreign wakeups.
        let (renderer, frames) = cx
            .update_window(handle, |_, window, cx| {
                ExtentPreview::request_basemap(&ExtentMap::new([112., -44., 154., -10.]), renderer, window, cx).unwrap()
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !frames
            .try_recv()
            .is_ok_and(|frame| cx.update(|cx| preview.update(cx, |preview, cx| preview.apply_frame(frame, cx))))
        {
            assert!(Instant::now() < deadline, "The preview basemap did not settle in time");
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(renderer);
        assert!(frames.is_closed(), "Dropping the preview worker must join it");
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let image = preview.read(cx).image().expect("A settled frame must supply the basemap");
            let rendered = image.size(0);
            let scale = window.scale_factor();
            assert_eq!(
                (i32::from(rendered.width), i32::from(rendered.height)),
                ((640. * scale) as i32, (320. * scale) as i32),
                "The basemap renders at the fixed preview size and the display's pixel density"
            );
            let pixels = image.as_bytes(0).unwrap();
            assert!(
                pixels.chunks(4).all(|pixel| pixel == [240, 240, 240, 255]),
                "The local style background must fill the frame"
            );
            assert!(window.try_find("dataset-extent-basemap-unavailable").is_none());
        })
        .unwrap();
    }
}
