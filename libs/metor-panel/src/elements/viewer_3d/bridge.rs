//! Bevy bridge: a single process-wide [`bevy::App`] owned directly by the
//! GPUI thread. There are no command channels and no dedicated render
//! thread — viewer mutations are synchronous ECS calls (or queued onto a
//! deferred-mutation list while the App is on a background worker for a
//! render), and renders are produced on demand by shipping the App to
//! `cx.background_executor()`.
//!
//! ## Threading model
//!
//! At rest the [`BevyBridge`] holds `Some(App)`. Mutations through
//! [`BevyBridge::with_world`] borrow the world and run inline. When a
//! viewer needs a frame the GPUI thread `take`s the App, ships it to a
//! background-executor task that calls `app.update()` once, then restores
//! the App on the main thread (draining any mutations that arrived while
//! it was gone).
//!
//! Bevy's `App` is `Send` at the type level via the unsafe `Send` impl on
//! `World`. The runtime safety guarantee is that no `NonSend<T>` resource
//! is ever accessed from a thread other than the one that owns it. Our
//! plugin set deliberately omits anything window/audio/winit-related, so
//! we believe there are no non-send resources in play. If a future plugin
//! addition introduces one, the second render will land on a different
//! pool worker and panic — fail loud, fail fast.
//!
//! ## Frame distribution
//!
//! The Bevy world owns a [`LatestFrames`] resource that the readback
//! observer writes into every time a `ReadbackComplete` event fires. After
//! `app.update()` returns, the background task drains that resource into
//! a viewer-id keyed map of (width, height, tight bytes), wraps each into
//! a `gpui::RenderImage`, and returns them along with the App. The main
//! thread then stores the new frames into per-viewer slots and notifies
//! every live viewer entity to repaint.

use std::collections::HashMap;
use std::sync::Arc;

use bevy::app::SubApps;
use bevy::ecs::world::World;
use gpui::{AsyncApp, BorrowAppContext, Global, RenderImage, Task, WeakEntity};
use smallvec::SmallVec;

use super::{Viewer3d, bevy_app};

/// Stable identifier for a live viewer. Minted by [`BevyBridge::register`]
/// and never reused for the lifetime of the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ViewerId(pub u64);

/// Stable identifier for a model within one viewer. Minted by the owning
/// `Viewer3d` and never reused. Bevy entities carry a `ModelTag(ModelId)`
/// component so per-model lookups go through tag queries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ModelId(pub u64);

/// A deferred world mutation. Queued on [`BevyBridge`] when a mutation is
/// requested while the App is on a background worker; drained when the
/// App is restored.
type WorldOp = Box<dyn FnOnce(&mut World) + 'static>;

/// Per-viewer state stored on the GPUI side. The actual ECS entities live
/// in the Bevy world; the slot is just bookkeeping the GPUI thread can
/// read without taking the App.
pub struct ViewerSlot {
    pub id: ViewerId,
    /// Most recent size requested by the viewer (after quantization).
    /// `(width, height)` in physical pixels.
    pub requested_size: (u32, u32),
    /// Latest fully-rendered frame, ready for `Window::paint_image`.
    /// `None` until the first render completes.
    pub frame: Option<Arc<RenderImage>>,
}

/// Process-wide bridge to the Bevy app. Stored as a `gpui::Global` and
/// lazy-initialized on the first viewer creation.
pub struct BevyBridge {
    /// The Bevy `SubApps`. `Some` while it lives on the GPUI thread,
    /// `None` while a background-executor task is running an
    /// `sub_apps.update()`. We hold `SubApps` rather than `App` because
    /// `App` contains a non-`Send` `RunnerFn` we don't use.
    app: Option<SubApps>,
    /// Per-viewer bookkeeping (size, latest frame, etc.).
    viewers: HashMap<ViewerId, ViewerSlot>,
    /// Mutations queued while `app` is `None`. Drained inside
    /// [`Self::restore_app`] before the App is reinserted.
    pending: Vec<WorldOp>,
    next_viewer_id: u64,
    /// Weak refs to live viewer entities. After a render completes the
    /// bridge calls `notify` on each so they repaint with their new
    /// frame. Stale entries are pruned lazily.
    viewer_entities: SmallVec<[WeakEntity<Viewer3d>; 4]>,
    /// Whether a render task is currently in flight. Set when a render is
    /// scheduled; cleared in [`Self::restore_app`].
    render_in_flight: bool,
    /// Most recently spawned render task. Held to keep it alive (gpui
    /// `Task` cancels on drop).
    _render_task: Option<Task<()>>,
}

impl Global for BevyBridge {}

impl BevyBridge {
    /// Fetch or create the process-wide bridge. The first call builds
    /// the Bevy `SubApps` synchronously on the GPUI thread.
    pub fn get_or_init(cx: &mut gpui::App) -> &mut BevyBridge {
        if !cx.has_global::<BevyBridge>() {
            let app = bevy_app::build_app();
            cx.set_global(BevyBridge {
                app: Some(app),
                viewers: HashMap::new(),
                pending: Vec::new(),
                next_viewer_id: 1,
                viewer_entities: SmallVec::new(),
                render_in_flight: false,
                _render_task: None,
            });
        }
        cx.global_mut::<BevyBridge>()
    }

    /// Apply a mutation to the Bevy world. Runs inline if the `SubApps`
    /// is on the GPUI thread; otherwise queues the closure and applies
    /// it when the `SubApps` is restored after a render.
    pub fn with_world<F>(&mut self, f: F)
    where
        F: FnOnce(&mut World) + 'static,
    {
        if let Some(app) = self.app.as_mut() {
            f(app.main.world_mut());
        } else {
            self.pending.push(Box::new(f));
        }
    }

    /// Read the most recent frame for a viewer, if any.
    pub fn frame_for(&self, id: ViewerId) -> Option<Arc<RenderImage>> {
        self.viewers.get(&id).and_then(|s| s.frame.clone())
    }

    /// Register a fresh viewer: mint a new id, allocate a `ViewerSlot`,
    /// and queue (or run inline) the world mutations that spawn the
    /// camera, light, and render target inside the world.
    ///
    /// Returns the freshly-minted [`ViewerId`].
    pub fn register(
        &mut self,
        weak: WeakEntity<Viewer3d>,
        width: u32,
        height: u32,
    ) -> ViewerId {
        let id = ViewerId(self.next_viewer_id);
        self.next_viewer_id += 1;
        self.viewers.insert(
            id,
            ViewerSlot {
                id,
                requested_size: (width, height),
                frame: None,
            },
        );
        self.viewer_entities.push(weak);
        self.with_world(move |world| {
            bevy_app::create_viewer(world, id, width, height);
        });
        id
    }

    /// Tear down a viewer: drop its slot, despawn its ECS entities.
    pub fn unregister(&mut self, id: ViewerId) {
        self.viewers.remove(&id);
        self.with_world(move |world| {
            bevy_app::despawn_viewer(world, id);
        });
    }

    /// Resize a viewer's render target. The slot's `requested_size` is
    /// updated immediately; the world mutation is queued.
    pub fn resize(&mut self, id: ViewerId, width: u32, height: u32) {
        if let Some(slot) = self.viewers.get_mut(&id) {
            if slot.requested_size == (width, height) {
                return;
            }
            slot.requested_size = (width, height);
        }
        self.with_world(move |world| {
            bevy_app::resize_viewer(world, id, width, height);
        });
    }

    /// Update a viewer's camera pose.
    pub fn set_camera(
        &mut self,
        id: ViewerId,
        target: glam::Vec3,
        yaw: f32,
        pitch: f32,
        distance: f32,
        fov_y_rad: f32,
    ) {
        self.with_world(move |world| {
            bevy_app::set_viewer_camera(world, id, target, yaw, pitch, distance, fov_y_rad);
        });
    }

    /// Add or replace a model within a viewer.
    pub fn load_model(
        &mut self,
        viewer_id: ViewerId,
        model_id: ModelId,
        path: std::path::PathBuf,
    ) {
        self.with_world(move |world| {
            bevy_app::load_model(world, viewer_id, model_id, path);
        });
    }

    /// Despawn one model from a viewer.
    pub fn remove_model(&mut self, viewer_id: ViewerId, model_id: ModelId) {
        self.with_world(move |world| {
            bevy_app::remove_model(world, viewer_id, model_id);
        });
    }

    /// Apply a live binding delta to one model. `None` axes are left at
    /// identity.
    pub fn set_live_transform(
        &mut self,
        viewer_id: ViewerId,
        model_id: ModelId,
        translation: Option<glam::Vec3>,
        rotation: Option<glam::Quat>,
    ) {
        self.with_world(move |world| {
            bevy_app::set_live_transform(world, viewer_id, model_id, translation, rotation);
        });
    }

    /// Schedule a render if one isn't already in flight. The render
    /// runs on a background-executor worker; on completion every live
    /// viewer entity is notified to repaint.
    ///
    /// Structure: an outer foreground task holds the (`!Send`) `AsyncApp`
    /// for cx access, and inside it we `await` an inner background-task
    /// that owns the `SubApps` and runs `update()`. Only the inner future
    /// needs to be `Send` (it captures only `SubApps` and `viewer_dims`,
    /// both of which are `Send`).
    pub fn schedule_render(cx: &mut gpui::App) {
        // Take the SubApps out of the bridge on the GPUI thread. This
        // closure also marks the bridge as "render in flight" so a
        // second concurrent caller is a no-op.
        let take_result = cx.update_global::<BevyBridge, _>(|bridge, _cx| {
            if bridge.render_in_flight {
                return None;
            }
            let app = bridge.app.take()?;
            bridge.render_in_flight = true;
            let viewer_dims: HashMap<ViewerId, (u32, u32)> = bridge
                .viewers
                .iter()
                .map(|(&id, slot)| (id, slot.requested_size))
                .collect();
            Some((app, viewer_dims))
        });
        let Some((app, viewer_dims)) = take_result else {
            return;
        };

        let task = cx.spawn(async move |cx: &mut AsyncApp| {
            // Run app.update() + readback on a background worker. The
            // bg future captures `app` (Send) and `viewer_dims` (Send).
            let bg = cx.background_executor().spawn(async move {
                let mut app = app;
                app.update();
                let raw_frames = bevy_app::take_latest_frames(app.main.world_mut());
                let mut wrapped: HashMap<ViewerId, Arc<RenderImage>> =
                    HashMap::with_capacity(raw_frames.len());
                for (id, padded) in raw_frames {
                    let Some(&(w, h)) = viewer_dims.get(&id) else {
                        continue;
                    };
                    let tight = bevy_app::strip_row_padding(&padded, w, h);
                    if let Some(image) = super::make_render_image_from_bytes(w, h, tight) {
                        wrapped.insert(id, image);
                    }
                }
                (app, wrapped)
            });
            let (app, wrapped) = bg.await;

            // Restore the SubApps + store frames + notify all live
            // viewer entities, all on the GPUI thread.
            let _ = cx.update(|cx| {
                let entities_to_notify: SmallVec<[WeakEntity<Viewer3d>; 4]> =
                    cx.update_global::<BevyBridge, _>(|bridge, _cx| {
                        bridge.restore_app(app);
                        for (id, image) in wrapped {
                            if let Some(slot) = bridge.viewers.get_mut(&id) {
                                slot.frame = Some(image);
                            }
                        }
                        bridge.render_in_flight = false;
                        bridge.viewer_entities.clone()
                    });
                for weak in entities_to_notify {
                    let _ = weak.update(cx, |_, cx| cx.notify());
                }
            });
        });

        cx.update_global::<BevyBridge, _>(|bridge, _cx| {
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
    }

    /// Prune dropped weak references. Called occasionally to keep the
    /// notify list bounded; not strictly necessary for correctness.
    pub fn prune_dead_viewers(&mut self) {
        self.viewer_entities.retain(|w| w.upgrade().is_some());
    }
}
