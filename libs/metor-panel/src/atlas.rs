//! Deferred release of [`RenderImage`]s from a window's sprite atlas.
//!
//! Plots, maps and the 3D viewer swap in a fresh `RenderImage` for every
//! frame they draw and hand the previous one to `window.drop_image`. gpui frees
//! the atlas slot at once, but the Metal renderer keeps up to three drawables
//! in flight and uploads new sprites with a CPU-side `replace_region` that does
//! not wait for them. With many plots repainting together, the next upload
//! lands in a slot an earlier, still-executing frame samples, and that frame
//! shows one plot's pixels inside another's bounds.
//!
//! Releasing through [`release_image`] instead parks the image until enough
//! frames have passed for every command buffer that could reference it to
//! finish. Each window keeps its own queue, because each window owns its atlas.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{App, Global, RenderImage, Window, WindowId};

/// Frame ticks a released image waits out. Metal's layer holds three
/// drawables, so at most two frames after the last one that painted the image
/// can still be on the GPU; one more covers the frame being built now.
pub(crate) const SETTLE_FRAMES: u32 = 4;

/// Floor on the wait, for ticks that request no draw and so put no
/// back-pressure on the drawable pool.
pub(crate) const SETTLE_TIME: Duration = Duration::from_millis(50);

struct Parked {
    image: Arc<RenderImage>,
    since: Instant,
    frames: u32,
}

/// One window's parked images, independent of gpui so the bookkeeping can be
/// tested without a window.
#[derive(Default)]
struct Queue(Vec<Parked>);

impl Queue {
    fn park(&mut self, image: Arc<RenderImage>, now: Instant) {
        self.0.push(Parked {
            image,
            since: now,
            frames: 0,
        });
    }

    /// Counts one frame against every parked image and returns the ones that
    /// have waited out both the frame budget and [`SETTLE_TIME`].
    fn tick(&mut self, now: Instant) -> Vec<Arc<RenderImage>> {
        let mut settled = Vec::new();
        self.0.retain_mut(|parked| {
            parked.frames += 1;
            let done = parked.frames >= SETTLE_FRAMES && now - parked.since >= SETTLE_TIME;
            if done {
                settled.push(parked.image.clone());
            }
            !done
        });
        settled
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Default)]
struct AtlasReleases {
    windows: HashMap<WindowId, Queue>,
}

impl Global for AtlasReleases {}

/// Drop `image` from `window`'s sprite atlas once no in-flight frame can
/// still be sampling it.
pub(crate) fn release_image(window: &mut Window, cx: &mut App, image: Arc<RenderImage>) {
    let id = window.window_handle().window_id();
    let queue = cx
        .default_global::<AtlasReleases>()
        .windows
        .entry(id)
        .or_default();
    let schedule = queue.is_empty();
    queue.park(image, Instant::now());
    if schedule {
        window.on_next_frame(settle);
    }
}

fn settle(window: &mut Window, cx: &mut App) {
    let id = window.window_handle().window_id();
    let releases = cx.default_global::<AtlasReleases>();
    let Some(queue) = releases.windows.get_mut(&id) else {
        return;
    };
    let settled = queue.tick(Instant::now());
    if queue.is_empty() {
        releases.windows.remove(&id);
    } else {
        window.on_next_frame(settle);
    }
    for image in settled {
        let _ = window.drop_image(image);
    }
}

/// Drop `images` from every window's atlas once in-flight frames are done
/// with them, for an owner that is going away without a window at hand (a
/// torn-out tab can have painted into several windows).
pub(crate) fn release_orphans(cx: &mut App, images: Vec<Arc<RenderImage>>) {
    if images.is_empty() {
        return;
    }
    let wait = cx.background_executor().timer(ORPHAN_WAIT);
    cx.spawn(async move |cx| {
        wait.await;
        cx.update(|cx| {
            for image in images {
                cx.drop_image(image, None);
            }
        });
    })
    .detach();
}

/// Wait before dropping an orphan: [`SETTLE_FRAMES`] frames at 30 Hz.
pub(crate) const ORPHAN_WAIT: Duration = Duration::from_millis(150);

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext, div, prelude::*};
    use image::Frame;
    use smallvec::smallvec;

    struct Empty;

    impl Render for Empty {
        fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
            div()
        }
    }

    fn parked(cx: &mut VisualTestContext) -> usize {
        cx.update(|window, cx| {
            cx.try_global::<AtlasReleases>()
                .and_then(|r| r.windows.get(&window.window_handle().window_id()))
                .map_or(0, Queue::len)
        })
    }

    #[gpui::test]
    fn images_wait_out_in_flight_frames(cx: &mut TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| Empty);
        let image = Arc::new(RenderImage::new(smallvec![Frame::new(
            image::RgbaImage::new(1, 1)
        )]));
        cx.update(|window, cx| release_image(window, cx, image));
        assert_eq!(parked(cx), 1);

        for _ in 0..SETTLE_FRAMES - 1 {
            cx.update(|window, cx| window.simulate_next_frame(cx));
        }
        assert_eq!(parked(cx), 1, "released before the frame budget ran out");

        std::thread::sleep(SETTLE_TIME);
        cx.update(|window, cx| window.simulate_next_frame(cx));
        assert_eq!(parked(cx), 0);
    }

    fn one_pixel() -> Arc<RenderImage> {
        Arc::new(RenderImage::new(smallvec![Frame::new(
            image::RgbaImage::new(1, 1)
        )]))
    }

    /// An image leaves the queue only after both the frame budget and the
    /// time floor, and each image counts from when it was parked.
    #[test]
    fn queue_releases_after_frames_and_time() {
        let start = Instant::now();
        let mut queue = Queue::default();
        let (a, b) = (one_pixel(), one_pixel());
        queue.park(a.clone(), start);
        for _ in 0..SETTLE_FRAMES - 1 {
            assert!(queue.tick(start + SETTLE_TIME).is_empty());
        }
        queue.park(b.clone(), start + SETTLE_TIME);
        let settled = queue.tick(start + SETTLE_TIME);
        assert_eq!(settled.len(), 1);
        assert!(Arc::ptr_eq(&settled[0], &a));

        // Frames alone are not enough without the time floor.
        for _ in 0..SETTLE_FRAMES {
            assert!(queue.tick(start + SETTLE_TIME).is_empty());
        }
        let settled = queue.tick(start + SETTLE_TIME * 2);
        assert!(Arc::ptr_eq(&settled[0], &b));
        assert!(queue.is_empty());
    }
}
