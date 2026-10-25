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
const SETTLE_FRAMES: u32 = 4;

/// Floor on the wait, for ticks that request no draw and so put no
/// back-pressure on the drawable pool.
const SETTLE_TIME: Duration = Duration::from_millis(50);

struct Parked {
    image: Arc<RenderImage>,
    since: Instant,
    frames: u32,
}

#[derive(Default)]
struct AtlasReleases {
    windows: HashMap<WindowId, Vec<Parked>>,
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
    queue.push(Parked {
        image,
        since: Instant::now(),
        frames: 0,
    });
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
    let now = Instant::now();
    let mut settled = Vec::new();
    queue.retain_mut(|parked| {
        parked.frames += 1;
        let done = parked.frames >= SETTLE_FRAMES && now - parked.since >= SETTLE_TIME;
        if done {
            settled.push(parked.image.clone());
        }
        !done
    });
    if queue.is_empty() {
        releases.windows.remove(&id);
    } else {
        window.on_next_frame(settle);
    }
    for image in settled {
        let _ = window.drop_image(image);
    }
}

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
                .map_or(0, Vec::len)
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
}
