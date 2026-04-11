//! Process-wide holder for the headless Bevy app driving every 3D viewer.
//!
//! The bridge is intentionally dumb. It owns:
//!
//! - the [`SubApps`] (the headless Bevy world),
//! - a queue of deferred world mutations that arrived while the app was
//!   on a background-executor worker for a render,
//! - the in-flight render task and a flag to de-dupe scheduling,
//! - an orphan list of `RenderImage`s that need to be released from
//!   gpui's sprite atlas after their owning viewer has been dropped,
//! - a monotonic counter minting per-viewer [`RenderLayers`] slots.
//!
//! It does **not** track which viewers exist, their sizes, their frames,
//! or which ones are dirty. Every viewer owns its own ECS entity handles
//! (`camera`, `light`, `readback`) and issues direct `with_world(|w| …)`
//! calls to mutate them — no per-operation wrapper methods.
//!
//! ## Threading model
//!
//! At rest `app: Some(SubApps)`. When a viewer's prepaint schedules a
//! render we `take` the `SubApps`, ship it to a background-executor task
//! that calls `sub_apps.update()` once, then restore it on the GPUI
//! thread. While the app is taken, viewer mutations are boxed up into
//! `pending` and drained onto the main world the moment the app comes
//! back.
//!
//! Bevy's `App` is `Send` at the type level via the unsafe `Send` impl on
//! `World`. The runtime safety guarantee is that no `NonSend<T>` resource
//! is ever accessed from a thread other than the one that owns it. Our
//! plugin set deliberately omits anything window/audio/winit-related. If
//! a future plugin addition introduces a non-send resource the second
//! render will land on a different pool worker and panic — fail loud.

use std::sync::Arc;

use bevy::app::SubApps;
use bevy::camera::visibility::RenderLayers;
use bevy::ecs::world::World;
use gpui::{AsyncApp, BorrowAppContext, Global, RenderImage, Task};

use super::bevy_app;

/// A deferred world mutation. Queued when a mutation arrives while the
/// [`SubApps`] is on a background-executor worker for a render; drained
/// onto the main world in [`BevyBridge::restore_app`].
type WorldOp = Box<dyn FnOnce(&mut World) + Send + 'static>;

/// Process-wide bridge to the headless Bevy app. Stored as a
/// [`gpui::Global`] and lazy-initialized on the first viewer creation.
pub struct BevyBridge {
    /// The headless Bevy `SubApps`. `Some` while it lives on the GPUI
    /// thread, `None` while a background-executor task is running a
    /// `sub_apps.update()`.
    app: Option<SubApps>,
    /// Mutations queued while `app` is `None`. Drained inside
    /// [`Self::restore_app`] before the app is reinserted.
    pending: Vec<WorldOp>,
    /// `true` while a background render task is in flight. Set by
    /// [`Self::schedule_render`], cleared by [`Self::restore_app`].
    render_in_flight: bool,
    /// Most recently spawned render task. Held to keep it alive (gpui
    /// `Task` cancels on drop).
    _render_task: Option<Task<()>>,
    /// Sprite-atlas entries owned by viewers that have already been
    /// unregistered. Drained opportunistically by any live viewer's next
    /// prepaint (which has `&mut Window` access to call `drop_image`).
    /// Grows only in the pathological case where every viewer is dropped
    /// at once; in that case the remaining entries leak until the
    /// process exits — a bounded, rare leak we accept.
    pending_releases: Vec<Arc<RenderImage>>,
    /// Monotonic counter minting the [`RenderLayers`] slot for each new
    /// viewer. Layer 0 is intentionally unused (it's the default layer
    /// for any untagged entity, which we don't want any viewer's camera
    /// to see) so the counter starts at 1.
    next_render_layer: usize,
}

impl Global for BevyBridge {}

impl BevyBridge {
    /// Fetch or create the process-wide bridge. The first call builds
    /// the headless `SubApps` synchronously on the GPUI thread.
    pub fn get_or_init(cx: &mut gpui::App) -> &mut BevyBridge {
        if !cx.has_global::<BevyBridge>() {
            let app = bevy_app::build_app();
            cx.set_global(BevyBridge {
                app: Some(app),
                pending: Vec::new(),
                render_in_flight: false,
                _render_task: None,
                pending_releases: Vec::new(),
                next_render_layer: 1,
            });
        }
        cx.global_mut::<BevyBridge>()
    }

    /// Apply a mutation to the Bevy world. Runs inline if the `SubApps`
    /// is on the GPUI thread; otherwise queues the closure and applies
    /// it when the `SubApps` is restored after a render.
    pub fn with_world<F>(&mut self, f: F)
    where
        F: FnOnce(&mut World) + Send + 'static,
    {
        if let Some(app) = self.app.as_mut() {
            f(app.main.world_mut());
        } else {
            self.pending.push(Box::new(f));
        }
    }

    /// Claim the next [`RenderLayers`] slot for a freshly-created viewer.
    /// Slots are never reused, mirroring the old `ViewerId` lifetime.
    pub fn claim_render_layer(&mut self) -> usize {
        let layer = self.next_render_layer;
        self.next_render_layer += 1;
        layer
    }

    /// Park an `Arc<RenderImage>` that belonged to a now-dead viewer so
    /// some surviving viewer can release it from gpui's sprite atlas on
    /// its next prepaint. Called from `Viewer3d::on_release`.
    pub fn orphan_release(&mut self, image: Arc<RenderImage>) {
        self.pending_releases.push(image);
    }

    /// Drain any orphaned atlas entries. Called from a live viewer's
    /// prepaint, which is the only place we have `&mut Window` access.
    pub fn take_orphaned_releases(&mut self) -> Vec<Arc<RenderImage>> {
        std::mem::take(&mut self.pending_releases)
    }

    /// Spawn a render task if one isn't already in flight. The
    /// background worker runs `sub_apps.update()` once — nothing else.
    /// Frame transport happens inside the update via the per-viewer
    /// [`bevy_app::FrameSink`] thingbufs, so the bridge never touches
    /// frame bytes.
    ///
    /// After the render completes the task restores the `SubApps` onto
    /// the GPUI thread (draining any queued mutations as it goes) and
    /// calls `cx.refresh()` so every live viewer's next prepaint picks
    /// up whatever new frames landed in its queue.
    pub fn schedule_render(cx: &mut gpui::App) {
        let app = cx.update_global::<BevyBridge, _>(|bridge, _| {
            if bridge.render_in_flight {
                return None;
            }
            let app = bridge.app.take()?;
            bridge.render_in_flight = true;
            Some(app)
        });
        let Some(app) = app else {
            return;
        };

        let task = cx.spawn(async move |cx: &mut AsyncApp| {
            let app = cx
                .background_executor()
                .spawn(async move {
                    let mut app = app;
                    app.update();
                    app
                })
                .await;

            let _ = cx.update(|cx| {
                cx.update_global::<BevyBridge, _>(|bridge, _| {
                    bridge.restore_app(app);
                });
                cx.refresh_windows();
            });
        });

        cx.update_global::<BevyBridge, _>(|bridge, _| {
            bridge._render_task = Some(task);
        });
    }

    /// Restore the `SubApps` after a background render. Drains queued
    /// mutations onto the main world before reinserting them.
    fn restore_app(&mut self, mut app: SubApps) {
        let pending: Vec<WorldOp> = std::mem::take(&mut self.pending);
        if !pending.is_empty() {
            let world = app.main.world_mut();
            for op in pending {
                op(world);
            }
        }
        self.app = Some(app);
        self.render_in_flight = false;
    }
}

/// Build the [`RenderLayers`] slot for a given viewer layer index. Kept
/// here so both the bridge and `Viewer3d` can reach it without pulling
/// `bevy::render` into every call site.
pub(super) fn render_layers_for(layer: usize) -> RenderLayers {
    RenderLayers::from_layers(&[layer])
}
