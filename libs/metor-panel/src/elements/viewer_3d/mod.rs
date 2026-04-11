//! 3D scene viewer element. A single Bevy `App` runs on a dedicated thread
//! and is shared by every [`Viewer3d`] instance; communication is via
//! crossbeam channels managed by [`bridge::BevyBridge`].
//!
//! Each `Viewer3d` holds a [`ViewerHandle`], a pumping task that drains the
//! frame channel into an `Arc<RenderImage>`, and a canvas paint callback that
//! blits that image the same way the time-series plot does.

pub(crate) mod bevy_app;
pub mod bridge;

use std::sync::Arc;

use gpui::{
    Bounds, Context, Corners, IntoElement, Pixels, RenderImage, Window, canvas, div, prelude::*,
    px,
};
use image::{Frame, ImageBuffer, Rgba};
use smallvec::SmallVec;

use crate::theme::theme;

pub use bridge::{BevyBridge, ViewerCommand, ViewerFrame, ViewerHandle, ViewerId};

/// Default render target size used the first time a viewer is created, before
/// the first resize callback fires. Small enough to not waste VRAM, large
/// enough that the first frame isn't a pixelated mess.
const INITIAL_SIZE: (u32, u32) = (512, 512);

/// A single 3D scene embedded in a GPUI canvas. Frames come from the shared
/// Bevy app via [`ViewerHandle::frame_rx`]; rendering is handled by a pump
/// task that converts them into `Arc<RenderImage>` and stores them on the
/// element for the next paint.
pub struct Viewer3d {
    /// Kept alive for its Drop impl, which sends a `Despawn` command to the
    /// Bevy world. Phase 6 reads this to send `SetCamera` / transform updates.
    #[allow(dead_code)]
    handle: ViewerHandle,
    frame_image: Option<Arc<RenderImage>>,
    frame_size: (u32, u32),
    dropped_images: Vec<Arc<RenderImage>>,
    _pump: gpui::Task<()>,
}

impl Viewer3d {
    /// Register this viewer with the process-wide Bevy bridge and spawn the
    /// frame-pump task.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let handle = {
            let bridge = BevyBridge::get_or_init(cx);
            bridge.register(INITIAL_SIZE.0, INITIAL_SIZE.1)
        };
        let pump = Self::spawn_pump(&handle, cx);
        Self {
            handle,
            frame_image: None,
            frame_size: INITIAL_SIZE,
            dropped_images: Vec::new(),
            _pump: pump,
        }
    }

    /// Spawn a long-lived task that drains frames off the bridge channel and
    /// writes them to `frame_image`, triggering a repaint. Uses
    /// `background_executor` for the blocking `recv` so GPUI's main thread
    /// never blocks on the channel.
    fn spawn_pump(handle: &ViewerHandle, cx: &mut Context<Self>) -> gpui::Task<()> {
        let rx = handle.frame_rx().clone();
        cx.spawn(async move |this, cx| {
            loop {
                let recv_rx = rx.clone();
                let Some(frame) = cx
                    .background_executor()
                    .spawn(async move { recv_rx.recv().ok() })
                    .await
                else {
                    break;
                };
                let Some(image) = make_render_image(&frame) else {
                    continue;
                };
                let size = (frame.width, frame.height);
                if this
                    .update(cx, |this, cx| {
                        if let Some(prev) = this.frame_image.replace(image) {
                            this.dropped_images.push(prev);
                        }
                        this.frame_size = size;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
    }
}

/// Wrap tight BGRA bytes into a `gpui::RenderImage` suitable for
/// `Window::paint_image`.
fn make_render_image(frame: &ViewerFrame) -> Option<Arc<RenderImage>> {
    let buffer =
        ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(frame.width, frame.height, frame.rgba.clone())?;
    let frames = SmallVec::from_elem(Frame::new(buffer), 1);
    Some(Arc::new(RenderImage::new(frames)))
}

impl Render for Viewer3d {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        div()
            .size_full()
            .bg(theme.bg_primary)
            .child(
                canvas(
                    // Prepaint: capture the latest image + dropped-image list.
                    // Returning them as state makes the paint closure
                    // self-contained and lets us release old images by calling
                    // `window.drop_image` without touching element state.
                    {
                        let this = cx.entity().downgrade();
                        move |bounds: Bounds<Pixels>, _window, cx| {
                            let state = this
                                .update(cx, |this, _cx| {
                                    (
                                        this.frame_image.clone(),
                                        std::mem::take(&mut this.dropped_images),
                                    )
                                })
                                .unwrap_or((None, Vec::new()));
                            (bounds, state.0, state.1)
                        }
                    },
                    move |_, (bounds, image, dropped), window, _cx| {
                        for img in dropped {
                            let _ = window.drop_image(img);
                        }
                        if let Some(img) = image {
                            let _ = window.paint_image(bounds, Corners::default(), img, 0, false);
                        }
                    },
                )
                .size_full()
                .min_h(px(64.0)),
            )
    }
}
