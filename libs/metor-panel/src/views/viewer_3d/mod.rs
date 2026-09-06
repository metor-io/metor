//! Bevy-backed 3D scene viewer.
//!
//! One headless Bevy app, held inside [`bridge::BevyBridge`], renders
//! every [`Viewer3d`] in the panel. Each viewer owns:
//!
//! - A set of stable Bevy entities ([`ViewerEntities`]) for its camera,
//!   light, and readback sentinel. They are wrapped in
//!   `Arc<OnceLock<_>>` so mutation closures can run against the bridge's
//!   FIFO queue without racing construction.
//! - A depth-2 `thingbuf` queue that the readback observer writes into
//!   and the next prepaint drains; the `Vec<u8>` storage is reused in
//!   place for zero allocation in steady state.
//! - A `Vec<ModelEntry>`, each with its own entity cell and any
//!   component-stream tasks driving live transforms.
//!
//! The gpui element itself is a thin mouse/keyboard surface over a
//! canvas that blits the most recently received frame.

pub(crate) mod bevy_app;
pub mod bridge;
pub mod camera;

mod config;
pub use config::{CameraConfig, ModelConfig, Viewer3dPanelConfig};

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

#[allow(unused_imports)]
use crate::inspect;
use crate::theme::theme;
use crate::views::binding::{StreamUpdate, spawn_seeded_stream};

pub use bridge::BevyBridge;
pub use camera::OrbitCamera;

use bevy_app::{FrameQueue, FrameSink, LiveDelta, PropagateRenderLayers, new_frame_queue};

/// Swizzle a Z-down metor-panel `[x, y, z]` into Bevy's Y-up `Vec3`.
///
/// Missing elements default to `0.0`.
fn pick_position(view: &ComponentView<'_>) -> Vec3 {
    let pick = |i: usize| view.get(i).map(|v| v.as_f64() as f32).unwrap_or(0.0);
    let x = pick(0);
    let y = pick(1);
    let z = pick(2);
    Vec3::new(x, z, -y)
}

/// Swizzle a Z-down quaternion `[i, j, k, w]` into Bevy's Y-up `Quat`.
///
/// Applies the same axis swap as [`pick_position`] to the imaginary
/// parts. Does not re-normalize the result.
fn pick_attitude(view: &ComponentView<'_>) -> Quat {
    let pick = |i: usize| view.get(i).map(|v| v.as_f64() as f32).unwrap_or(0.0);
    let i = pick(0);
    let j = pick(1);
    let k = pick(2);
    let w = pick(3);
    Quat::from_xyzw(i, k, -j, w)
}

const INITIAL_SIZE: (u32, u32) = (512, 512);

/// Force-render window after a new model loads. GLTF finishes asynchronously
/// and the PBR pipeline needs a few frames for lighting to settle.
const POST_LOAD_WINDOW: Duration = Duration::from_millis(500);

/// Bevy-side identities that define a single viewer's render target.
///
/// The entity ids are stable even across render-target resizes, so
/// mutation closures can cache them freely.
#[derive(Component)]
struct ReadbackChild(Entity);

#[derive(Clone, Copy)]
struct ViewerEntities {
    camera: Entity,
    light: Entity,
    /// Hosts both the [`Readback`] component and the [`FrameSink`] the
    /// readback observer writes into.
    readback: Entity,
}

/// 3D scene element.
///
/// Owns the reflected config (models, camera FOV), the frame queue that
/// receives Bevy-side renders, and the per-model tracking state used by
/// [`Viewer3d::reconcile`] to spawn and tear down world entities.
#[derive(facet::Facet)]
pub struct Viewer3d {
    #[facet(opaque)]
    entities: Arc<OnceLock<ViewerEntities>>,
    #[facet(skip)]
    render_layer: usize,
    #[facet(opaque)]
    db: Option<Arc<DB>>,
    #[facet(opaque)]
    _binding_changes: gpui::Task<()>,
    #[facet(opaque)]
    camera: OrbitCamera,

    pub models: Vec<gpui::Entity<ModelEntry>>,
    #[facet(inspect::range(min = "0.1", max = "3.141592653589793"))]
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
    #[facet(skip)]
    temporal_revision: u64,
    #[facet(skip)]
    frame_generation: u64,
    #[facet(opaque)]
    loading_until: Option<Instant>,
    #[facet(opaque)]
    drag: Option<DragState>,
    #[facet(opaque)]
    tracking: HashMap<EntityId, ModelTracking>,
    #[facet(opaque)]
    last_camera: CameraSnapshot,
}

/// Background tracking for one model entry.
///
/// `path` and `*_binding` record the values used to spawn the current
/// Bevy state. Reconcile compares the stored values against the entry's
/// live fields and respawns or rebinds on any change.
struct ModelTracking {
    entity: Arc<OnceLock<Entity>>,
    path: String,
    position_binding: Option<ComponentId>,
    orientation_binding: Option<ComponentId>,
    tasks: SmallVec<[gpui::Task<()>; 2]>,
    /// Child-to-parent observer. Dropping unwires the notify propagation
    /// when the tracker is removed.
    _subscription: Subscription,
}

/// Previous values of the reflected camera scalars.
///
/// Compared during reconcile so inspector edits to `camera_fov` push
/// through to Bevy's `Projection`.
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

/// Pure-config description of one model in a [`Viewer3d`].
///
/// The live Bevy entity, streaming tasks, and resolved components live
/// in [`ModelTracking`] on the parent viewer, keyed by this entry's
/// gpui `EntityId`.
#[derive(facet::Facet)]
#[facet(pod)]
pub struct ModelEntry {
    pub label: SharedString,
    pub path: String,
    pub position_binding: crate::data_binding::Binding,
    pub orientation_binding: crate::data_binding::Binding,
}

impl ModelEntry {
    pub fn position_binding_component(&self) -> Option<ComponentId> {
        (!self.position_binding.is_unbound()).then_some(self.position_binding.id())
    }

    pub fn orientation_binding_component(&self) -> Option<ComponentId> {
        (!self.orientation_binding.is_unbound()).then_some(self.orientation_binding.id())
    }

    pub fn empty() -> Self {
        Self {
            label: SharedString::new_static(""),
            path: String::new(),
            position_binding: crate::data_binding::Binding::default(),
            orientation_binding: crate::data_binding::Binding::default(),
        }
    }
}

/// Snapshot taken at drag start so pose deltas apply to the pre-drag
/// camera rather than accumulating from frame to frame.
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
    /// Build a viewer without a DB connection.
    ///
    /// Models can still load from the filesystem, but component-driven
    /// bindings are unavailable.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self::new_inner(None, cx)
    }

    /// Build a viewer bound to `db` so model transforms can follow
    /// component values.
    pub fn with_db(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        Self::new_inner(Some(db), cx)
    }

    fn new_inner(db: Option<Arc<DB>>, cx: &mut Context<Self>) -> Self {
        let entities: Arc<OnceLock<ViewerEntities>> = Arc::new(OnceLock::new());
        let frame_queue = new_frame_queue();

        let render_layer = {
            let bridge = BevyBridge::or_init(cx);
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
                            generation: 0,
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

        // The bridge runs ops FIFO, so the entity cells set during
        // construction are guaranteed populated by the time this
        // release op executes.
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
            _binding_changes: db
                .as_ref()
                .map(|db| crate::data_binding::watch_registrations(db.clone(), cx))
                .unwrap_or_else(|| gpui::Task::ready(())),
            db,
            camera,
            models: Vec::new(),
            camera_fov,
            requested_size: INITIAL_SIZE,
            frame_queue,
            current_frame: None,
            pending_release: None,
            needs_render: true,
            temporal_revision: 0,
            frame_generation: 0,
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

    /// Mark the viewer dirty and schedule a repaint.
    fn mark_dirty(&mut self, cx: &mut Context<Self>) {
        self.needs_render = true;
        cx.notify();
    }

    /// Run `f` on the Bevy world once this viewer's entities exist.
    ///
    /// Cells are guaranteed populated by FIFO bridge ordering. Marks the
    /// viewer dirty so the next prepaint submits a frame.
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

    /// Bring Bevy-side state in sync with the reflected config.
    ///
    /// Runs on every self-notify, including notifications proxied by the
    /// per-tracker subscription. Every branch is snapshot-diffed so
    /// idempotent reconciles do no work.
    ///
    /// Dirties the viewer by flipping `needs_render` directly; calling
    /// `cx.notify()` here would re-enter this function forever.
    fn reconcile(&mut self, cx: &mut Context<Self>) {
        let mut work = false;

        // Drop trackers for removed models; subscriptions and binding
        // tasks follow the tracker into the grave.
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

        if let Some(db) = &self.db {
            for model in &self.models {
                model.update(cx, |model, cx| {
                    model.position_binding.resolve(db, cx);
                    model.orientation_binding.resolve(db, cx);
                });
            }
        }
        // Snapshot up-front so we can borrow `self.tracking` mutably
        // without aliasing `self.models`.
        let layer = self.render_layer;
        let db = self.db.clone();
        type ModelSnapshot = (
            EntityId,
            gpui::Entity<ModelEntry>,
            String,
            Option<ComponentId>,
            Option<ComponentId>,
        );
        let model_snapshots: Vec<ModelSnapshot> = self
            .models
            .iter()
            .map(|m| {
                let id = m.entity_id();
                let entry = m.read(cx);
                (
                    id,
                    m.clone(),
                    entry.path.clone(),
                    entry.position_binding_component(),
                    entry.orientation_binding_component(),
                )
            })
            .collect();

        for (id, model, path, pos_bind, orient_bind) in model_snapshots {
            // First-sight insert: wire the subscription so inspector
            // edits on the entry proxy back into reconcile.
            if let std::collections::hash_map::Entry::Vacant(slot) = self.tracking.entry(id) {
                let subscription = cx.observe(&model, |this, _, cx| {
                    this.reconcile(cx);
                });
                slot.insert(ModelTracking {
                    entity: Arc::new(OnceLock::new()),
                    path: String::new(),
                    position_binding: None,
                    orientation_binding: None,
                    tasks: SmallVec::new(),
                    _subscription: subscription,
                });
                work = true;
            }

            // Path change: respawn the Bevy entity into a fresh cell
            // and clear bindings — the old cell is gone, so the tasks
            // that captured it must be rebuilt. Zeroing the stored
            // bindings triggers the rebind branch below on this pass.
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
                                world.entity_mut(e).insert(LiveDelta {
                                    position_missing: pos_bind.is_some(),
                                    rotation_missing: orient_bind.is_some(),
                                    ..LiveDelta::default()
                                });
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
                            |v, delta| {
                                delta.translation = v;
                                delta.position_missing = v.is_none();
                            },
                        ));
                    }
                    if let Some(component_id) = orient_bind {
                        tasks.push(Self::spawn_binding_stream(
                            db,
                            entity_cell,
                            component_id,
                            cx,
                            pick_attitude,
                            |q, delta| {
                                delta.rotation = q;
                                delta.rotation_missing = q.is_none();
                            },
                        ));
                    }
                    let track = self.tracking.get_mut(&id).expect("inserted above");
                    track.tasks = tasks;
                }
                work = true;
            }
        }

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

    /// Append a model entry; the Bevy spawn happens in [`Self::reconcile`].
    pub fn add_model(
        &mut self,
        label: impl Into<SharedString>,
        path: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        let entry = ModelEntry {
            label: label.into(),
            path: path.into(),
            position_binding: crate::data_binding::Binding::default(),
            orientation_binding: crate::data_binding::Binding::default(),
        };
        self.models.push(cx.new(|_| entry));
        cx.notify();
    }

    /// Remove a model; reconcile tears down its Bevy entity and tasks.
    pub fn remove_model(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.models.len() {
            return;
        }
        self.models.remove(index);
        cx.notify();
    }

    /// Change a model's GLTF path; reconcile despawns and respawns.
    pub fn set_model_path(&mut self, index: usize, path: String, cx: &mut Context<Self>) {
        let Some(entry) = self.models.get(index).cloned() else {
            return;
        };
        entry.update(cx, |model, cx| {
            model.path = path;
            cx.notify();
        });
    }

    /// Change a model's display label.
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

    /// Snap the target size to a 64-pixel grid.
    ///
    /// 64 × 4 bytes per pixel = 256-byte rows, which matches wgpu's
    /// `COPY_BYTES_PER_ROW_ALIGNMENT` so readback becomes a flat memcpy.
    /// The grid also prevents resize thrash during sub-pixel edge drags.
    fn quantize(size: gpui::Size<Pixels>, scale: f32) -> (u32, u32) {
        const STEP: u32 = 64;
        const MIN: u32 = 64;
        let phys_w = (f32::from(size.width) * scale).max(1.0) as u32;
        let phys_h = (f32::from(size.height) * scale).max(1.0) as u32;
        let round = |v: u32| v.div_ceil(STEP) * STEP;
        (round(phys_w).max(MIN), round(phys_h).max(MIN))
    }

    /// Allocate a new render target when `new_size` differs from the
    /// current one.
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
            let id = world
                .get::<ReadbackChild>(ents.readback)
                .map_or(ents.readback, |c| c.0);
            let mut readback = world.entity_mut(id);
            readback.insert(Readback::texture(new_handle));
            if let Some(mut sink) = readback.get_mut::<FrameSink>() {
                sink.size = (w, h);
            }
        });
    }

    /// A new readback entity prevents an in-flight image from an earlier seek
    /// being tagged as the current pose when its GPU callback finally arrives.
    fn invalidate_time_frame(&mut self, cx: &mut Context<Self>) {
        let revision = crate::temporal::snapshot(cx).map_or(0, |s| s.revision);
        if revision == self.temporal_revision {
            return;
        }
        self.temporal_revision = revision;
        self.frame_generation = self.frame_generation.wrapping_add(1);
        self.pending_release = self.current_frame.take();
        let generation = self.frame_generation;
        self.with_entities(cx, move |world, ents| {
            let old = world
                .get::<ReadbackChild>(ents.readback)
                .map_or(ents.readback, |c| c.0);
            let Some(readback) = world.get::<Readback>(old).cloned() else {
                return;
            };
            let Some(sink) = world.get::<FrameSink>(old) else {
                return;
            };
            let sink = FrameSink {
                queue: sink.queue.clone(),
                size: sink.size,
                generation,
            };
            if old == ents.readback {
                world.entity_mut(old).remove::<(Readback, FrameSink)>();
            } else {
                world.despawn(old);
            }
            let child = world.spawn((readback, sink, ChildOf(ents.readback))).id();
            world.entity_mut(ents.readback).insert(ReadbackChild(child));
        });
    }

    /// Flush the current [`OrbitCamera`] pose to the Bevy camera entity.
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

    /// Restore the camera to its default orbit pose.
    pub fn reset_camera(&mut self, cx: &mut Context<Self>) {
        self.camera = OrbitCamera::default();
        self.sync_camera(cx);
    }

    /// Pipe a component stream into one model's [`LiveDelta`].
    ///
    /// Shared between the position and orientation binding paths; the
    /// per-axis behavior lives in the `extract` and `apply` fn pointers.
    fn spawn_binding_stream<T: Send + 'static>(
        db: Arc<DB>,
        entity_cell: Arc<OnceLock<Entity>>,
        component_id: ComponentId,
        cx: &mut Context<Self>,
        extract: fn(&ComponentView<'_>) -> T,
        apply: fn(Option<T>, &mut LiveDelta),
    ) -> gpui::Task<()> {
        spawn_seeded_stream(
            db,
            component_id,
            cx,
            move |_, _| ((), move |view| Some(extract(&view))),
            move |this, update, cx| {
                let value = match update {
                    StreamUpdate::Value(value) => Some(value),
                    StreamUpdate::Unavailable => None,
                    _ => return,
                };
                let cell = entity_cell.clone();
                cx.update_global::<BevyBridge, _>(|bridge, _| {
                    bridge.with_world(move |world| {
                        let Some(entity) = cell.get().copied() else {
                            return;
                        };
                        let mut delta = world.get::<LiveDelta>(entity).copied().unwrap_or_default();
                        apply(value, &mut delta);
                        world.entity_mut(entity).insert(delta);
                    });
                });
                this.mark_dirty(cx);
            },
        )
    }
}

/// Spawn the Bevy-side entity for one model.
///
/// An empty `path` still produces an entity carrying a default transform
/// and [`LiveDelta`] so later binding updates have somewhere to land.
fn spawn_model(world: &mut World, render_layer: usize, path: &str) -> Entity {
    let layers = bridge::render_layers_for(render_layer);
    if path.is_empty() {
        return world
            .spawn((
                Transform::IDENTITY,
                LiveDelta::default(),
                Visibility::Inherited,
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
            Visibility::Inherited,
            layers,
            PropagateRenderLayers,
        ))
        .id()
}

/// Wrap tightly-packed BGRA bytes into a [`RenderImage`] for
/// `Window::paint_image`.
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
    /// Promote the next queued frame into `current_frame`, parking the
    /// old frame in `pending_release` for atlas cleanup on the next paint.
    ///
    /// Invariant: `pending_release` is `None` on entry — prepaint drains
    /// it immediately after calling this.
    fn consume_frame(&mut self) {
        let Some(mut slot) = self.frame_queue.pop_ref() else {
            return;
        };
        if slot.generation != self.frame_generation {
            return;
        }
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

                            // Capture the frame plus any queued
                            // releases, then request a render when dirty.
                            let (frame, releases) = this
                                .update(cx, |this, cx| {
                                    this.invalidate_time_frame(cx);
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
