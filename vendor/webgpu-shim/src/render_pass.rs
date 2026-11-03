//! Attachment ownership and scoped private-depth passes for the native bridge.

// The legacy FFI module disables Clippy globally; keep this new code checked.
#![warn(clippy::all)]

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::{Arc, Mutex};

use crate::{WGPUCommandEncoderImpl, WGPURenderPassEncoderImpl, generated_bindings::*};

pub(crate) struct PassState {
    // Keep the encoder alive even if native releases its own handle first.
    parent: Arc<WGPUCommandEncoderImpl>,
    attachments: Attachments,
    pub(crate) raster: Mutex<RasterState>,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct RasterState {
    pub(crate) viewport: Option<[f32; 6]>,
    pub(crate) scissor: Option<[u32; 4]>,
}

impl RasterState {
    fn apply(self, pass: &mut wgpu::RenderPass<'_>) {
        if let Some([x, y, width, height, min, max]) = self.viewport {
            pass.set_viewport(x, y, width, height, min, max);
        }
        if let Some([x, y, width, height]) = self.scissor {
            pass.set_scissor_rect(x, y, width, height);
        }
    }
}

struct ColorAttachment {
    view: wgpu::TextureView,
    resolve: Option<wgpu::TextureView>,
    depth_slice: Option<u32>,
    ops: wgpu::Operations<wgpu::Color>,
}

struct DepthStencilAttachment {
    view: wgpu::TextureView,
    depth: Option<wgpu::Operations<f32>>,
    stencil: Option<wgpu::Operations<u32>>,
}

pub(crate) struct Attachments {
    colors: Vec<Option<ColorAttachment>>,
    depth_stencil: Option<DepthStencilAttachment>,
    no_extensions: bool,
}

impl Attachments {
    pub(crate) unsafe fn from_descriptor(desc: &WGPURenderPassDescriptor) -> Self {
        let colors = if desc.colorAttachmentCount == 0 || desc.colorAttachments.is_null() {
            &[][..]
        } else {
            // SAFETY: the C ABI caller provides the descriptor's attachment array.
            unsafe { std::slice::from_raw_parts(desc.colorAttachments, desc.colorAttachmentCount) }
        };
        // SAFETY: the optional attachment is valid for this call.
        let depth_stencil = unsafe { desc.depthStencilAttachment.as_ref() };
        let no_extensions = desc.nextInChain.is_null()
            && colors.iter().all(|color| color.nextInChain.is_null())
            && depth_stencil.is_none_or(|depth| depth.nextInChain.is_null());
        Self {
            colors: colors.iter().map(|color| unsafe { ColorAttachment::from_native(color) }).collect(),
            depth_stencil: depth_stencil.map(|depth| unsafe { DepthStencilAttachment::from_native(depth) }),
            no_extensions,
        }
    }

    pub(crate) fn begin(&self, encoder: &mut wgpu::CommandEncoder, reload: bool) -> wgpu::RenderPass<'static> {
        let colors: Vec<_> = self
            .colors
            .iter()
            .map(|color| {
                color.as_ref().map(|color| wgpu::RenderPassColorAttachment {
                    view: &color.view,
                    resolve_target: color.resolve.as_ref(),
                    depth_slice: color.depth_slice,
                    ops: operations(color.ops, reload),
                })
            })
            .collect();
        let depth_stencil_attachment = self.depth_stencil.as_ref().map(|depth| wgpu::RenderPassDepthStencilAttachment {
            view: &depth.view,
            depth_ops: depth.depth.map(|ops| operations(ops, reload)),
            stencil_ops: depth.stencil.map(|ops| operations(ops, reload)),
        });
        encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                color_attachments: &colors,
                depth_stencil_attachment,
                ..Default::default()
            })
            .forget_lifetime()
    }

    fn private_color(&self) -> Option<&ColorAttachment> {
        if !self.no_extensions || self.colors.len() != 1 {
            return None;
        }
        let color = self.colors[0].as_ref()?;
        if !color.supports_private_depth() || !self.depth_stencil.as_ref().is_none_or(DepthStencilAttachment::supports_private_depth) {
            return None;
        }
        Some(color)
    }
}

fn operations<T: Copy>(ops: wgpu::Operations<T>, reload: bool) -> wgpu::Operations<T> {
    wgpu::Operations {
        load: if reload { wgpu::LoadOp::Load } else { ops.load },
        store: ops.store,
    }
}

fn stored<T>(ops: Option<wgpu::Operations<T>>) -> bool {
    ops.is_none_or(|ops| ops.store == wgpu::StoreOp::Store)
}

fn single_sampled(view: &wgpu::TextureView) -> bool {
    view.texture().sample_count() == 1 && view.texture().dimension() == wgpu::TextureDimension::D2
}

fn target_size(view: &wgpu::TextureView) -> [u32; 2] {
    let size = view.texture().size();
    [size.width, size.height]
}

fn usable_private_depth(view: &wgpu::TextureView, color: &ColorAttachment) -> bool {
    single_sampled(view)
        && view.texture().format() == wgpu::TextureFormat::Depth32Float
        && view.texture().usage().contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
        && target_size(view) == target_size(&color.view)
}

fn native_store(store: WGPUStoreOp) -> wgpu::StoreOp {
    if store == WGPUStoreOp_Discard {
        wgpu::StoreOp::Discard
    } else {
        wgpu::StoreOp::Store
    }
}

impl ColorAttachment {
    fn supports_private_depth(&self) -> bool {
        let texture = self.view.texture();
        // View subresources are opaque; only these textures have an unambiguous extent.
        self.resolve.is_none()
            && self.depth_slice.is_none()
            && self.ops.store == wgpu::StoreOp::Store
            && single_sampled(&self.view)
            && texture.mip_level_count() == 1
            && texture.size().depth_or_array_layers == 1
    }

    unsafe fn from_native(color: &WGPURenderPassColorAttachment) -> Option<Self> {
        // SAFETY: native attachment handles are valid for the begin-pass call.
        let view = unsafe { color.view.as_ref() }?;
        let resolve = unsafe { color.resolveTarget.as_ref() }.map(|view| view.0.clone());
        let clear = color.clearValue;
        Some(Self {
            view: view.0.clone(),
            resolve,
            depth_slice: (color.depthSlice != u32::MAX).then_some(color.depthSlice),
            ops: wgpu::Operations {
                load: if color.loadOp == WGPULoadOp_Clear {
                    wgpu::LoadOp::Clear(wgpu::Color {
                        r: clear.r,
                        g: clear.g,
                        b: clear.b,
                        a: clear.a,
                    })
                } else {
                    wgpu::LoadOp::Load
                },
                store: native_store(color.storeOp),
            },
        })
    }
}

impl DepthStencilAttachment {
    fn supports_private_depth(&self) -> bool {
        single_sampled(&self.view) && stored(self.depth) && stored(self.stencil)
    }

    unsafe fn from_native(depth: &WGPURenderPassDepthStencilAttachment) -> Self {
        // SAFETY: native supplies a live depth/stencil attachment view.
        let view = unsafe { &*depth.view };
        Self {
            view: view.0.clone(),
            depth: (depth.depthReadOnly == 0).then_some(wgpu::Operations {
                load: if depth.depthLoadOp == WGPULoadOp_Clear {
                    wgpu::LoadOp::Clear(depth.depthClearValue)
                } else {
                    wgpu::LoadOp::Load
                },
                store: native_store(depth.depthStoreOp),
            }),
            stencil: (depth.stencilReadOnly == 0).then_some(wgpu::Operations {
                load: if depth.stencilLoadOp == WGPULoadOp_Clear {
                    wgpu::LoadOp::Clear(depth.stencilClearValue)
                } else {
                    wgpu::LoadOp::Load
                },
                store: native_store(depth.stencilStoreOp),
            }),
        }
    }
}

impl PassState {
    pub(crate) fn new(parent: Arc<WGPUCommandEncoderImpl>, attachments: Attachments) -> Self {
        Self {
            parent,
            attachments,
            raster: Mutex::new(RasterState::default()),
        }
    }
}

/// Returns the native color attachment's actual pixel extent if private depth is supported.
///
/// Uses the same native attachment and handle-state checks as
/// [`with_render_pass_with_depth`], including an active pass, unpoisoned locks,
/// and a usable parent encoder. Call this before allocating private depth to
/// avoid allocations for unsupported passes and logical/HiDPI size rounding.
/// The eligible color texture has exactly one mip and layer, so its width and
/// height equal the effective view extent despite opaque view metadata.
///
/// This is read-only: it allocates no GPU resources, ends no pass, and records
/// or submits no commands. It does not reserve support for a later scope or
/// validate the caller's eventual private depth view.
///
/// # Safety
/// `encoder` must be a live shim render-pass handle, exclusively accessible to
/// the calling thread during this call. Neither this handle nor its parent
/// encoder may be accessed concurrently or from an existing pass callback.
#[must_use]
pub unsafe fn private_depth_target_size(encoder: usize) -> Option<[u32; 2]> {
    // SAFETY: the caller guarantees a live, exclusive shim handle.
    let handle = unsafe { &*(encoder as *const WGPURenderPassEncoderImpl) };
    let pass = handle.0.lock().ok()?;
    pass.as_ref()?;
    let color = handle.1.attachments.private_color()?;
    let _raster = handle.1.raster.lock().ok()?;
    let parent = handle.1.parent.0.lock().ok()?;
    parent.as_ref()?;
    Some(target_size(&color.view))
}

/// Draws in a scoped private-depth pass on the native pass's command encoder.
///
/// Returns `false` without ending the native pass or calling `draw` if it is
/// ended/poisoned, or its attachments are unsupported. Supported passes have
/// exactly one single-sampled 2D color attachment backed by a single-mip,
/// single-layer texture, no resolve/depth slice or descriptor extensions, and
/// Store for every writable color/depth/stencil aspect. Read-only depth/stencil
/// aspects remain read-only on continuation.
/// The private texture must be single-sampled Depth32Float with RENDER_ATTACHMENT
/// usage and width/height matching [`private_depth_target_size`]. Other texture
/// formats/sample counts/usages or mismatched sizes also return `false`.
///
/// The private pass loads/stores the same color and clears `depth` to 0.0 (with
/// Store). The caller chooses its pipeline's depth comparison, normally Greater.
/// Native attachments resume with Load and their original store/read-only state,
/// on the SAME FFI handle, without submitting. C-ABI viewport/scissor state is
/// applied to both passes; other pipeline/resource/dynamic state must be rebound.
/// Callback panics propagate after restoring the native pass, without poisoning
/// its locks. Already recorded callback draws are not rolled back.
///
/// # Safety
/// `encoder` must be a live shim render-pass handle, exclusively used for this
/// call. Neither the callback nor another thread may access this handle or its
/// parent encoder (including finishing/submitting it). `draw` must not end,
/// replace, or forget the borrowed pass, or leave GPU queries/debug groups open.
/// `depth` must be a live, same-device, nonaliasing 2D render view selecting one
/// mip/layer with the SAME effective extent as the color view.
/// These view/device properties are caller obligations: wgpu does not expose
/// view subresource metadata. Invalid GPU commands/device loss are not fallback
/// cases; ordinary wgpu validation still applies.
#[must_use]
pub unsafe fn with_render_pass_with_depth(encoder: usize, depth: &wgpu::TextureView, draw: impl FnOnce(&mut wgpu::RenderPass<'_>)) -> bool {
    // SAFETY: the caller guarantees a live, exclusive shim handle.
    let handle = unsafe { &*(encoder as *const WGPURenderPassEncoderImpl) };
    let Ok(mut pass) = handle.0.lock() else { return false };
    let Some(color) = handle.1.attachments.private_color() else { return false };
    if pass.is_none() || !usable_private_depth(depth, color) {
        return false;
    }
    let Ok(raster) = handle.1.raster.lock().map(|state| *state) else {
        return false;
    };
    let Ok(mut parent) = handle.1.parent.0.lock() else { return false };
    let Some(parent_encoder) = parent.as_mut() else { return false };
    // Catch inside the lock guards, so a callback panic cannot poison C-ABI locks.
    let result = catch_unwind(AssertUnwindSafe(|| {
        let restore = RestorePass {
            pass: &mut pass,
            encoder: parent_encoder,
            attachments: &handle.1.attachments,
            raster,
        };
        drop(restore.pass.take());
        let mut private = begin_private(restore.encoder, color, depth);
        raster.apply(&mut private);
        draw(&mut private);
        // `private` ends before `restore`, including during callback unwinding.
    }));
    drop(parent);
    drop(pass);
    if let Err(panic) = result {
        resume_unwind(panic);
    }
    true
}

fn begin_private(encoder: &mut wgpu::CommandEncoder, color: &ColorAttachment, depth: &wgpu::TextureView) -> wgpu::RenderPass<'static> {
    encoder
        .begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Native custom layer private depth"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color.view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: depth,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(0.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        })
        .forget_lifetime()
}

struct RestorePass<'a> {
    pass: &'a mut Option<wgpu::RenderPass<'static>>,
    encoder: &'a mut wgpu::CommandEncoder,
    attachments: &'a Attachments,
    raster: RasterState,
}

impl Drop for RestorePass<'_> {
    fn drop(&mut self) {
        let mut continuation = self.attachments.begin(self.encoder, true);
        self.raster.apply(&mut continuation);
        *self.pass = Some(continuation);
    }
}

#[cfg(test)]
#[path = "render_pass_tests.rs"]
mod tests;
