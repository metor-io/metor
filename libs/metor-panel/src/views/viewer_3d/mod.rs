//! 3D scene viewer element.
//!
//! A single headless Bevy [`bevy::app::App`] lives inside
//! [`bridge::BevyBridge`] (a `gpui::Global`) and is driven by periodic
//! renders from each [`Viewer3d`]'s prepaint closure. Every viewer owns:
//!
//! - A stable set of `Entity` handles inside the Bevy world
//!   ([`ViewerEntities`]): its camera, its directional light, and the
//!   readback sentinel that carries the per-viewer [`bevy_app::FrameSink`]
//!   component. The handles are wrapped in `Arc<OnceLock<_>>` so
//!   construction can be safely deferred through the bridge's pending-op
//!   queue — every mutation the viewer issues is a
//!   `bridge.with_world(|w| …)` closure that reads the cell, which FIFO
//!   ordering guarantees is filled by the time the closure runs.
//! - A depth-2 [`thingbuf::ThingBuf`] it shares with its `FrameSink`.
//!   The Bevy readback observer writes straight into this queue and the
//!   viewer's next prepaint drains it. The `Vec<u8>` storage is reused
//!   in place so steady-state rendering does zero per-frame allocations.
//! - A list of [`ModelEntry`]s, each with its own `Arc<OnceLock<Entity>>`
//!   and any component-stream tasks driving its live transform.
//!
//! The element itself is thin: a mouse/keyboard surface over a canvas
//! that blits whichever frame its receiver most recently produced.

pub(crate) mod bevy_app;
pub mod bridge;
pub mod camera;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bevy::asset::AssetServer;
use bevy::camera::RenderTarget;
use bevy::gltf::GltfAssetLabel;
use bevy::image::Image;
use bevy::prelude::*;
use bevy::render::gpu_readback::Readback;
use bevy::world_serialization::WorldAssetRoot;
use glam::{Quat, Vec3};
use gpui::{
    Bounds, Context, Corners, EntityId, IntoElement, MouseButton, Pixels, Point, RenderImage,
    SharedString, Subscription, Window, canvas, div, prelude::*, px,
};
use image::{Frame, ImageBuffer, Rgba};
use metor_db::DB;
use metor_proto::types::{ComponentId, ComponentView};
use smallvec::SmallVec;

use crate::theme::theme;
use crate::{AsComponentView, ComponentStream, ComponentStreamBuilder};

pub use bridge::BevyBridge;
pub use camera::OrbitCamera;

use bevy_app::{FrameQueue, FrameSink, LiveDelta, PropagateRenderLayers, new_frame_queue};

/// Convert a metor-panel `[x, y, z]` position component into a Bevy-frame
/// `Vec3`. metor-panel is Z-down; Bevy is Y-up, so position swizzles to
/// `(x, z, -y)`. Missing elements default to `0.0`.
fn pick_position(view: &ComponentView<'_>) -> Vec3 {
    let pick = |i: usize| view.get(i).map(|v| v.as_f64() as f32).unwrap_or(0.0);
    let x = pick(0);
    let y = pick(1);
    let z = pick(2);
    Vec3::new(x, z, -y)
}

/// Convert a metor-panel `[i, j, k, w]` quaternion component into a
/// Bevy-frame `Quat`. Same Z-down → Y-up swizzle as [`pick_position`],
/// applied to the imaginary parts: `(i, k, -j, w)`. Does not re-normalize.
fn pick_attitude(view: &ComponentView<'_>) -> Quat {
    let pick = |i: usize| view.get(i).map(|v| v.as_f64() as f32).unwrap_or(0.0);
    let i = pick(0);
    let j = pick(1);
    let k = pick(2);
    let w = pick(3);
    Quat::from_xyzw(i, k, -j, w)
}

/// Default render target size used the first time a viewer is created,
/// before the first canvas resize callback fires.
const INITIAL_SIZE: (u32, u32) = (512, 512);

/// How long after a model load we keep force-rendering every paint. The
/// GLTF loader completes asynchronously, so the scene tree and PBR
/// pipeline need a handful of frames to settle before the output is
/// stable.
const POST_LOAD_WINDOW: Duration = Duration::from_millis(500);

/// The three ECS entities that define one viewer's render target. Wrapped
/// in an `Arc<OnceLock<_>>` on [`Viewer3d`] so construction can be queued
/// behind an in-flight render — every mutator closure loads the cell and
/// FIFO op ordering guarantees it's filled by the time the closure runs.
#[derive(Clone, Copy)]
struct ViewerEntities {
    camera: Entity,
    light: Entity,
    /// Carries both the [`Readback`] component and the [`FrameSink`]
    /// that the `on_readback_complete` observer writes into. Stable
    /// across render-target resizes (the `Readback` component gets
    /// replaced in place; the entity id doesn't change).
    readback: Entity,
}

/// A 3D scene element. Owns a list of models, its own slice of the Bevy
/// world, and a depth-2 frame queue fed by the readback observer.
#[derive(facet::Facet)]
pub struct Viewer3d {
    #[facet(opaque)]
    entities: Arc<OnceLock<ViewerEntities>>,
    #[facet(skip)]
    render_layer: usize,
    #[facet(opaque)]
    db: Option<Arc<DB>>,
    #[facet(opaque)]
    camera: OrbitCamera,

    // Inspectable fields — reflected by the walker
    pub models: Vec<gpui::Entity<ModelEntry>>,
    pub camera_fov: f32,

    #[facet(skip)]
    requested_size: (u32, u32),
    #[facet(opaque)]
    frame_queue: FrameQueue,
    #[facet(opaque)]
    current_frame: Option<Arc<RenderImage>>,
    #[facet(opaque)]
    pending_release: Option<Arc<RenderImage>>,
    #[facet(skip)]
    needs_render: bool,
    #[facet(opaque)]
    loading_until: Option<Instant>,
    #[facet(opaque)]
    drag: Option<DragState>,
    #[facet(opaque)]
    tracking: HashMap<EntityId, ModelTracking>,
    #[facet(opaque)]
    last_camera: CameraSnapshot,
}

struct ModelTracking {
    entity: Arc<OnceLock<Entity>>,
    /// Last path we spawned for. Reconcile compares against the entry's
    /// current `path` and respawns the Bevy entity on any change.
    path: String,
    /// Last bindings we wired. Reconcile compares against the entry's
    /// current bindings and rebinds streaming tasks on any change.
    position_binding: Option<ComponentId>,
    orientation_binding: Option<ComponentId>,
    tasks: SmallVec<[gpui::Task<()>; 2]>,
    /// Drops when the tracker is removed, unwiring child→parent notify
    /// propagation for the now-removed model.
    _subscription: Subscription,
}

/// Snapshot of inspectable camera scalars used to detect inspector edits
/// between reconciles, so writes to `camera_fov` push through to the Bevy
/// `Projection`.
#[derive(Clone, Copy, PartialEq, Eq)]
struct CameraSnapshot {
    fov_bits: u32,
}

impl CameraSnapshot {
    fn from_fov(fov: f32) -> Self {
        Self {
            fov_bits: fov.to_bits(),
        }
    }
}

/// One model in a viewer. Pure config — the world-side `Entity`, binding
/// tasks, and resolved component live in the parent's
/// [`ModelTracking`] map keyed by this entry's gpui `EntityId`.
#[derive(facet::Facet)]
#[facet(pod)]
pub struct ModelEntry {
    pub label: SharedString,
    pub path: String,
    pub position_binding: Option<ComponentId>,
    pub orientation_binding: Option<ComponentId>,
}

impl ModelEntry {
    pub fn position_binding_component(&self) -> Option<ComponentId> {
        self.position_binding
    }

    pub fn orientation_binding_component(&self) -> Option<ComponentId> {
        self.orientation_binding
    }

    pub fn empty() -> Self {
        Self {
            label: SharedString::new_static(""),
            path: String::new(),
            position_binding: None,
            orientation_binding: None,
        }
    }
}

/// State captured at the start of a drag so the delta is applied to the
/// camera's pre-drag pose — avoids drift from accumulating pixel deltas
/// frame by frame.
#[derive(Clone, Copy)]
struct DragState {
    start_pos: Point<Pixels>,
    start_camera: OrbitCamera,
    mode: DragMode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DragMode {
    Rotate,
    Pan,
}

impl Viewer3d {
    /// Create a viewer with no DB. The viewer can still load models via
    /// [`Self::add_model`] but can't install component bindings.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self::new_inner(None, cx)
    }

    /// Create a viewer connected to `db` so the inspector can install
    /// position and orientation bindings from its components.
    pub fn with_db(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        Self::new_inner(Some(db), cx)
    }

    fn new_inner(db: Option<Arc<DB>>, cx: &mut Context<Self>) -> Self {
        let entities: Arc<OnceLock<ViewerEntities>> = Arc::new(OnceLock::new());
        let frame_queue = new_frame_queue();

        let render_layer = {
            let bridge = BevyBridge::get_or_init(cx);
            let layer = bridge.claim_render_layer();
            let layers = bridge::render_layers_for(layer);
            let entities_cell = entities.clone();
            let queue = frame_queue.clone();
            let (w, h) = INITIAL_SIZE;
            bridge.with_world(move |world| {
                let target_image = bevy_app::new_target_image(w, h);
                let target_handle = world.resource_mut::<Assets<Image>>().add(target_image);

                let light = world
                    .spawn((
                        DirectionalLight {
                            illuminance: 10_000.0,
                            ..default()
                        },
                        Transform::from_xyz(4.0, 8.0, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
                        layers.clone(),
                    ))
                    .id();

                let camera = world
                    .spawn((
                        Camera3d::default(),
                        Camera {
                            clear_color: ClearColorConfig::Custom(Color::srgb(0.05, 0.05, 0.08)),
                            ..default()
                        },
                        RenderTarget::from(target_handle.clone()),
                        Projection::Perspective(PerspectiveProjection {
                            fov: std::f32::consts::FRAC_PI_3,
                            ..default()
                        }),
                        Transform::from_xyz(3.0, 2.0, 3.0).looking_at(Vec3::ZERO, Vec3::Y),
                        layers.clone(),
                    ))
                    .id();

                let readback = world
                    .spawn((
                        Readback::texture(target_handle),
                        FrameSink {
                            queue,
                            size: (w, h),
                        },
                    ))
                    .id();

                let _ = entities_cell.set(ViewerEntities {
                    camera,
                    light,
                    readback,
                });
            });
            layer
        };

        // Queue a despawn op on drop. Because `with_world` is FIFO and
        // our spawn op was enqueued first, by the time this op runs the
        // entity cells are filled.
        let entities_for_release = entities.clone();
        cx.on_release(move |this, cx| {
            let ents = entities_for_release;
            let model_cells: Vec<Arc<OnceLock<Entity>>> =
                this.tracking.values().map(|t| t.entity.clone()).collect();
            cx.update_global::<BevyBridge, _>(|bridge, _| {
                for frame in this
                    .current_frame
                    .take()
                    .into_iter()
                    .chain(this.pending_release.take())
                {
                    bridge.orphan_release(frame);
                }
                bridge.with_world(move |world| {
                    let mut to_despawn: Vec<Entity> = Vec::new();
                    if let Some(e) = ents.get() {
                        to_despawn.extend([e.camera, e.light, e.readback]);
                    }
                    to_despawn.extend(model_cells.iter().filter_map(|c| c.get().copied()));
                    bevy_app::despawn_entities(world, &to_despawn);
                });
            });
        })
        .detach();

        let camera = OrbitCamera::default();
        let camera_fov = camera.fov_y_rad;
        cx.observe_self(Self::reconcile).detach();
        let mut viewer = Self {
            entities,
            render_layer,
            db,
            camera,
            models: Vec::new(),
            camera_fov,
            requested_size: INITIAL_SIZE,
            frame_queue,
            current_frame: None,
            pending_release: None,
            needs_render: true,
            loading_until: None,
            drag: None,
            tracking: HashMap::new(),
            last_camera: CameraSnapshot::from_fov(camera_fov),
        };
        viewer.sync_camera(cx);
        viewer
    }

    pub fn models(&self) -> &[gpui::Entity<ModelEntry>] {
        &self.models
    }

    pub fn camera(&self) -> OrbitCamera {
        self.camera
    }

    pub fn camera_mut(&mut self) -> &mut OrbitCamera {
        &mut self.camera
    }

    /// Mark the viewer as dirty so its next prepaint schedules a render
    /// and issue `cx.notify()` so gpui actually schedules that prepaint.
    fn mark_dirty(&mut self, cx: &mut Context<Self>) {
        self.needs_render = true;
        cx.notify();
    }

    /// Queue a world op against this viewer's own entities. The closure
    /// only runs once the construction-time spawn op has populated the
    /// cell, which FIFO ordering on the bridge guarantees. Marks the
    /// viewer dirty so the next prepaint schedules a render.
    fn with_entities(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut World, ViewerEntities) + Send + 'static,
    ) {
        let cell = self.entities.clone();
        cx.update_global::<BevyBridge, _>(|bridge, _| {
            bridge.with_world(move |world| {
                if let Some(&ents) = cell.get() {
                    f(world, ents);
                }
            });
        });
        self.mark_dirty(cx);
    }

    /// Reconcile the per-id [`ModelTracking`] map against the current
    /// `models` Vec. Runs on every self-notify (including child notifies
    /// proxied by the per-tracker subscription). Idempotent: branches
    /// are gated on snapshot diffs so a no-op reconcile does no work.
    ///
    /// Sets `needs_render = true` directly rather than calling
    /// `cx.notify()`, which would re-enter reconcile in an infinite loop.
    /// The notify that triggered this run already scheduled the next
    /// prepaint where `needs_render` will be observed.
    fn reconcile(&mut self, cx: &mut Context<Self>) {
        let mut work = false;

        // 1. Drop trackers for removed models. Subscription drops with
        //    the tracker; binding tasks drop with the SmallVec.
        let current_ids: HashSet<EntityId> = self.models.iter().map(|m| m.entity_id()).collect();
        let stale: Vec<EntityId> = self
            .tracking
            .keys()
            .copied()
            .filter(|id| !current_ids.contains(id))
            .collect();
        for id in stale {
            let track = self.tracking.remove(&id).expect("just enumerated");
            if let Some(entity) = track.entity.get().copied() {
                cx.update_global::<BevyBridge, _>(|bridge, _| {
                    bridge.with_world(move |world| {
                        world.despawn(entity);
                    });
                });
            }
            work = true;
        }

        // 2. Walk current models, creating trackers and reacting to
        //    path / binding changes. Snapshot up-front so we can borrow
        //    `self.tracking` mutably without aliasing `self.models`.
        let layer = self.render_layer;
        let db = self.db.clone();
        let model_snapshots: Vec<(
            EntityId,
            gpui::Entity<ModelEntry>,
            String,
            Option<ComponentId>,
            Option<ComponentId>,
        )> = self
            .models
            .iter()
            .map(|m| {
                let id = m.entity_id();
                let entry = m.read(cx);
                (
                    id,
                    m.clone(),
                    entry.path.clone(),
                    entry.position_binding,
                    entry.orientation_binding,
                )
            })
            .collect();

        for (id, model, path, pos_bind, orient_bind) in model_snapshots {
            // Insert tracker on first sight, wiring a child→parent
            // subscription so inspector edits to the entry trigger this
            // reconcile.
            if !self.tracking.contains_key(&id) {
                let subscription = cx.observe(&model, |this, _, cx| {
                    this.reconcile(cx);
                });
                self.tracking.insert(
                    id,
                    ModelTracking {
                        entity: Arc::new(OnceLock::new()),
                        path: String::new(),
                        position_binding: None,
                        orientation_binding: None,
                        tasks: SmallVec::new(),
                        _subscription: subscription,
                    },
                );
                work = true;
            }

            // Path change: despawn previous Bevy entity (if any) and
            // spawn a fresh one with a new cell. Bindings are also
            // cleared because they captured the old cell — they'll be
            // re-spawned by the bindings-changed branch below now that
            // `track.position_binding` / `orientation_binding` were
            // reset to `None`.
            let track = self.tracking.get_mut(&id).expect("inserted above");
            if track.path != path {
                let old_cell = std::mem::replace(&mut track.entity, Arc::new(OnceLock::new()));
                track.path = path.clone();
                track.tasks.clear();
                track.position_binding = None;
                track.orientation_binding = None;
                let new_cell = track.entity.clone();
                if !path.is_empty() {
                    self.loading_until = Some(Instant::now() + POST_LOAD_WINDOW);
                }
                let path_for_world = path;
                cx.update_global::<BevyBridge, _>(|bridge, _| {
                    bridge.with_world(move |world| {
                        if let Some(prev) = old_cell.get().copied() {
                            world.despawn(prev);
                        }
                        let _ = new_cell.set(spawn_model(world, layer, &path_for_world));
                    });
                });
                work = true;
            }

            // Bindings changed: drop old tasks, reset LiveDelta, spawn
            // fresh tasks against the (possibly fresh) entity cell.
            let track = self.tracking.get_mut(&id).expect("inserted above");
            let bindings_changed =
                track.position_binding != pos_bind || track.orientation_binding != orient_bind;
            if bindings_changed {
                track.tasks.clear();
                track.position_binding = pos_bind;
                track.orientation_binding = orient_bind;

                if let Some(db) = db.clone() {
                    let entity_cell = track.entity.clone();
                    let cell_for_reset = entity_cell.clone();
                    cx.update_global::<BevyBridge, _>(|bridge, _| {
                        bridge.with_world(move |world| {
                            if let Some(e) = cell_for_reset.get().copied() {
                                world.entity_mut(e).insert(LiveDelta::default());
                            }
                        });
                    });

                    let mut tasks: SmallVec<[gpui::Task<()>; 2]> = SmallVec::new();
                    if let Some(component_id) = pos_bind {
                        tasks.push(Self::spawn_binding_stream(
                            db.clone(),
                            entity_cell.clone(),
                            component_id,
                            cx,
                            pick_position,
                            |v, delta| delta.translation = Some(v),
                        ));
                    }
                    if let Some(component_id) = orient_bind {
                        tasks.push(Self::spawn_binding_stream(
                            db,
                            entity_cell,
                            component_id,
                            cx,
                            pick_attitude,
                            |q, delta| delta.rotation = Some(q),
                        ));
                    }
                    let track = self.tracking.get_mut(&id).expect("inserted above");
                    track.tasks = tasks;
                }
                work = true;
            }
        }

        // 3. Camera-fov edit detection.
        let snap = CameraSnapshot::from_fov(self.camera_fov);
        if snap != self.last_camera {
            self.camera.fov_y_rad = self.camera_fov;
            self.last_camera = snap;
            self.sync_camera(cx);
            work = true;
        }

        if work {
            self.needs_render = true;
        }
    }

    /// Append a new model. The Bevy-side spawn happens in [`Self::reconcile`]
    /// via the `cx.notify()` triggered by this push.
    pub fn add_model(
        &mut self,
        label: impl Into<SharedString>,
        path: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        let entry = ModelEntry {
            label: label.into(),
            path: path.into(),
            position_binding: None,
            orientation_binding: None,
        };
        self.models.push(cx.new(|_| entry));
        cx.notify();
    }

    /// Remove one model from the viewer. Reconcile despawns the Bevy
    /// entity and drops the binding tasks when the tracker disappears.
    pub fn remove_model(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.models.len() {
            return;
        }
        self.models.remove(index);
        cx.notify();
    }

    /// Update one model's GLTF path. The despawn-then-spawn happens in
    /// [`Self::reconcile`] via the per-model child subscription.
    pub fn set_model_path(&mut self, index: usize, path: String, cx: &mut Context<Self>) {
        let Some(entry) = self.models.get(index).cloned() else {
            return;
        };
        entry.update(cx, |model, cx| {
            model.path = path;
            cx.notify();
        });
    }

    /// Update one model's display label.
    pub fn set_model_label(
        &mut self,
        index: usize,
        label: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let label = label.into();
        if let Some(entry) = self.models.get(index) {
            entry.update(cx, |model, cx| {
                model.label = label;
                cx.notify();
            });
        }
    }

    /// Round a pixel size to a 64-pixel grid. The grid is a deliberate
    /// alignment: `64 * 4 = 256` bytes per row, which matches wgpu's
    /// `COPY_BYTES_PER_ROW_ALIGNMENT`. Readback rows need no padding and
    /// the observer's `copy_tight_rows` is a flat memcpy. Also avoids
    /// re-issuing resize ops on every sub-pixel edge drag.
    fn quantize(size: gpui::Size<Pixels>, scale: f32) -> (u32, u32) {
        const STEP: u32 = 64;
        const MIN: u32 = 64;
        let phys_w = (f32::from(size.width) * scale).max(1.0) as u32;
        let phys_h = (f32::from(size.height) * scale).max(1.0) as u32;
        let round = |v: u32| v.div_ceil(STEP) * STEP;
        (round(phys_w).max(MIN), round(phys_h).max(MIN))
    }

    /// If `new_size` differs from the last-requested size, swap the
    /// render target image and update the `FrameSink.size`.
    fn maybe_resize(&mut self, new_size: (u32, u32), cx: &mut Context<Self>) {
        if self.requested_size == new_size {
            return;
        }
        self.requested_size = new_size;
        let (w, h) = new_size;
        self.with_entities(cx, move |world, ents| {
            let new_image = bevy_app::new_target_image(w, h);
            let new_handle = world.resource_mut::<Assets<Image>>().add(new_image);
            world
                .entity_mut(ents.camera)
                .insert(RenderTarget::from(new_handle.clone()));
            let mut readback = world.entity_mut(ents.readback);
            readback.insert(Readback::texture(new_handle));
            if let Some(mut sink) = readback.get_mut::<FrameSink>() {
                sink.size = (w, h);
            }
        });
    }

    /// Push the current camera pose to the world.
    pub fn sync_camera(&mut self, cx: &mut Context<Self>) {
        let cam = self.camera;
        self.with_entities(cx, move |world, ents| {
            let eye = cam.eye();
            world
                .entity_mut(ents.camera)
                .insert(Transform::from_translation(eye).looking_at(cam.target, Vec3::Y))
                .insert(Projection::Perspective(PerspectiveProjection {
                    fov: cam.fov_y_rad,
                    ..default()
                }));
        });
    }

    /// Reset the camera to its default pose.
    pub fn reset_camera(&mut self, cx: &mut Context<Self>) {
        self.camera = OrbitCamera::default();
        self.sync_camera(cx);
    }

    /// Spawn a binding-stream task that pumps values from a component
    /// stream into the Bevy world's [`LiveDelta`] on one model. Shared
    /// by the position and orientation paths — the only per-axis
    /// knowledge lives in the `extract`/`apply` function pointers.
    fn spawn_binding_stream<T: Send + 'static>(
        db: Arc<DB>,
        entity_cell: Arc<OnceLock<Entity>>,
        component_id: ComponentId,
        cx: &mut Context<Self>,
        extract: fn(&ComponentView<'_>) -> T,
        apply: fn(T, &mut LiveDelta),
    ) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            let mut stream = component_id.into_stream(&db).await;
            loop {
                let value = extract(&stream.next().await.as_component_view());
                let cell = entity_cell.clone();
                let result = this.update(cx, |this, cx| {
                    cx.update_global::<BevyBridge, _>(|bridge, _| {
                        bridge.with_world(move |world| {
                            let Some(entity) = cell.get().copied() else {
                                return;
                            };
                            let mut delta =
                                world.get::<LiveDelta>(entity).copied().unwrap_or_default();
                            apply(value, &mut delta);
                            world.entity_mut(entity).insert(delta);
                        });
                    });
                    this.mark_dirty(cx);
                });
                if result.is_err() {
                    break;
                }
            }
        })
    }
}

/// Spawn a scene entity for one model. Returns the new [`Entity`]; the
/// caller writes it into its [`OnceLock`]. If `path` is empty the entity
/// still spawns with a default transform + [`LiveDelta`] so subsequent
/// binding updates have somewhere to land.
fn spawn_model(world: &mut World, render_layer: usize, path: &str) -> Entity {
    let layers = bridge::render_layers_for(render_layer);
    if path.is_empty() {
        return world
            .spawn((
                Transform::IDENTITY,
                LiveDelta::default(),
                layers,
                PropagateRenderLayers,
            ))
            .id();
    }
    let handle = world
        .resource::<AssetServer>()
        .load(GltfAssetLabel::Scene(0).from_asset(path.to_string()));
    world
        .spawn((
            WorldAssetRoot(handle),
            Transform::IDENTITY,
            LiveDelta::default(),
            layers,
            PropagateRenderLayers,
        ))
        .id()
}

/// Wrap tight BGRA bytes into a `gpui::RenderImage` suitable for
/// `Window::paint_image`. Called from the prepaint closure the moment a
/// fresh frame is popped out of the thingbuf.
fn make_render_image_from_bytes(
    width: u32,
    height: u32,
    rgba: Vec<u8>,
) -> Option<Arc<RenderImage>> {
    let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(width, height, rgba)?;
    let frames = SmallVec::from_elem(Frame::new(buffer), 1);
    Some(Arc::new(RenderImage::new(frames)))
}

impl Viewer3d {
    /// Pop the most recent frame (if any) out of the viewer's thingbuf
    /// into `self.current_frame`, parking the old frame in
    /// `self.pending_release` for atlas cleanup on the next paint.
    ///
    /// Invariant: `pending_release` is always `None` on entry because
    /// prepaint drains it immediately after calling this.
    fn consume_frame(&mut self) {
        let Some(mut slot) = self.frame_queue.pop_ref() else {
            return;
        };
        let (w, h) = (slot.width, slot.height);
        let bytes = std::mem::take(&mut slot.bytes);
        drop(slot);
        if let Some(image) = make_render_image_from_bytes(w, h, bytes) {
            self.pending_release = self.current_frame.replace(image);
        }
    }
}

impl Render for Viewer3d {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        div()
            .size_full()
            .bg(theme.bg_primary)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _window, cx| {
                    if event.click_count >= 2 {
                        this.reset_camera(cx);
                        cx.notify();
                        return;
                    }
                    let mode = if event.modifiers.shift {
                        DragMode::Pan
                    } else {
                        DragMode::Rotate
                    };
                    this.drag = Some(DragState {
                        start_pos: event.position,
                        start_camera: this.camera,
                        mode,
                    });
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &gpui::MouseDownEvent, _window, _cx| {
                    this.drag = Some(DragState {
                        start_pos: event.position,
                        start_camera: this.camera,
                        mode: DragMode::Pan,
                    });
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event: &gpui::MouseUpEvent, _window, _cx| {
                    this.drag = None;
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|this, _event: &gpui::MouseUpEvent, _window, _cx| {
                    this.drag = None;
                }),
            )
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, _window, cx| {
                    let Some(drag) = this.drag else {
                        return;
                    };
                    if !event.dragging() {
                        return;
                    }
                    let dx = f32::from(event.position.x - drag.start_pos.x);
                    let dy = f32::from(event.position.y - drag.start_pos.y);
                    let mut cam = drag.start_camera;
                    match drag.mode {
                        DragMode::Rotate => cam.rotate(dx, dy),
                        DragMode::Pan => cam.pan(dx, dy),
                    }
                    this.camera = cam;
                    this.sync_camera(cx);
                }),
            )
            .on_scroll_wheel(
                cx.listener(|this, event: &gpui::ScrollWheelEvent, _window, cx| {
                    let delta = event.delta.pixel_delta(px(20.0));
                    this.camera.zoom(-f32::from(delta.y));
                    this.sync_camera(cx);
                    cx.stop_propagation();
                }),
            )
            .child(
                canvas(
                    {
                        let this = cx.entity().downgrade();
                        move |bounds: Bounds<Pixels>, window, cx| {
                            let scale = window.scale_factor();
                            let new_size = Viewer3d::quantize(bounds.size, scale);

                            // Snapshot the frame + any atlas entries to
                            // release, then schedule a render if the
                            // viewer thinks it still needs one.
                            let (frame, releases) = this
                                .update(cx, |this, cx| {
                                    this.maybe_resize(new_size, cx);
                                    this.consume_frame();

                                    let mut releases =
                                        cx.update_global::<BevyBridge, _>(|bridge, _| {
                                            bridge.take_orphaned_releases()
                                        });
                                    releases.extend(this.pending_release.take());

                                    let loading_active =
                                        this.loading_until.is_some_and(|t| Instant::now() < t);
                                    if !loading_active {
                                        this.loading_until = None;
                                    }
                                    if this.needs_render || loading_active {
                                        this.needs_render = false;
                                        BevyBridge::schedule_render(cx);
                                    }

                                    (this.current_frame.clone(), releases)
                                })
                                .unwrap_or((None, Vec::new()));

                            for img in releases {
                                let _ = window.drop_image(img);
                            }

                            (bounds, frame)
                        }
                    },
                    move |_, (bounds, image), window, _cx| {
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
