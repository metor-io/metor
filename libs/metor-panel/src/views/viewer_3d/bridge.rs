//! Process-wide holder for the Bevy app shared by every 3D viewer.
//!
//! The bridge is intentionally minimal: it owns the [`SubApps`], a queue
//! of deferred world mutations, the in-flight render task, an orphan
//! list of atlas entries pending release, and a counter that mints
//! [`RenderLayers`] slots.
//!
//! Threading model:
//! - At rest the `SubApps` lives on the gpui thread and `with_world`
//!   runs synchronously.
//! - A scheduled render takes the app, ships it to a background
//!   worker for one `sub_apps.update()`, and restores it when done.
//!   World mutations that arrive while the app is off-thread queue in
//!   `pending` and drain just before the app is reinserted.
//!
//! Bevy's `App` is `Send` because `World` is unsafe-`Send`. The runtime
//! invariant is that no `NonSend<T>` resource ever crosses threads; the
//! shipped plugin set omits window, audio, and winit resources so this
//! holds. A future plugin that reintroduces a non-send resource will
//! panic loudly on the next render, which is the failure mode we want.

use std::sync::Arc;

use bevy::app::SubApps;
use bevy::camera::visibility::RenderLayers;
use bevy::ecs::world::World;
use gpui::{AsyncApp, BorrowAppContext, Global, RenderImage, Task};

use super::bevy_app;

/// Deferred world mutation. Queued while the app is off-thread, drained
/// by [`BevyBridge::restore_app`] just before the app is reinserted.
type WorldOp = Box<dyn FnOnce(&mut World) + Send + 'static>;

/// Global bridge to the shared Bevy app.
pub struct BevyBridge {
    /// The headless Bevy app. `None` while a background render is
    /// updating it.
    app: Option<SubApps>,
    pending: Vec<WorldOp>,
    /// True while a background render is outstanding; used to de-dupe
    /// scheduling.
    render_in_flight: bool,
    /// Holds the most recent render task alive; gpui tasks cancel on drop.
    _render_task: Option<Task<()>>,
    /// Atlas entries orphaned by viewer drops, released by any surviving
    /// viewer's next prepaint. The list only grows in the pathological
    /// case of every viewer dying at once, where the remainder leaks
    /// until shutdown — a bounded, accepted leak.
    pending_releases: Vec<Arc<RenderImage>>,
    /// Monotonic layer counter. Starts at 1 because layer 0 is Bevy's
    /// default for untagged entities and must not be visible to any
    /// per-viewer camera.
    next_render_layer: usize,
}

impl Global for BevyBridge {}

impl BevyBridge {
    /// Fetch the bridge, building the Bevy app on the first call.
    pub fn or_init(cx: &mut gpui::App) -> &mut BevyBridge {
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

    /// Apply `f` to the Bevy world.
    ///
    /// Runs inline when the app is on the gpui thread; otherwise queues
    /// for execution when the app returns. Operations run FIFO, which
    /// callers rely on to order construction before subsequent mutations.
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

    /// Reserve a fresh [`RenderLayers`] slot for a new viewer. Slots are
    /// never reused.
    pub fn claim_render_layer(&mut self) -> usize {
        let layer = self.next_render_layer;
        self.next_render_layer += 1;
        layer
    }

    /// Hand off a [`RenderImage`] owned by a dying viewer. A live
    /// viewer will release it from the sprite atlas on its next prepaint.
    pub fn orphan_release(&mut self, image: Arc<RenderImage>) {
        self.pending_releases.push(image);
    }

    /// Drain orphaned atlas entries. Must run from a viewer prepaint;
    /// that's the only place with `&mut Window` for `drop_image`.
    pub fn take_orphaned_releases(&mut self) -> Vec<Arc<RenderImage>> {
        std::mem::take(&mut self.pending_releases)
    }

    /// Schedule one background render, ignoring the call if one is
    /// already in flight.
    ///
    /// The worker runs a single `sub_apps.update()`; frame bytes flow
    /// through per-viewer [`bevy_app::FrameSink`] queues so the bridge
    /// never inspects them. On completion the task restores the app,
    /// drains queued mutations, and refreshes every gpui window so
    /// viewers pick up the new frames.
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

    /// Reinstate the app after a render. Queued mutations run first so
    /// callers never observe a gap where their ops are lost.
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

/// Construct the [`RenderLayers`] component for one viewer layer.
pub(super) fn render_layers_for(layer: usize) -> RenderLayers {
    RenderLayers::from_layers(&[layer])
}
