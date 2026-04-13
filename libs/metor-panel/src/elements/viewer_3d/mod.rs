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
    Bounds, Context, Corners, IntoElement, MouseButton, Pixels, Point, RenderImage, SharedString,
    Window, canvas, div, prelude::*, px,
};
use image::{Frame, ImageBuffer, Rgba};
use metor_db::DB;
use metor_proto::types::ComponentView;
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
}

/// One model in a viewer. Owns its path, label, bindings, and streaming
/// tasks. The world-side `Entity` is created by a queued spawn op and
/// written into `entity` once; later mutations load it the same way the
/// viewer's own entities are loaded.
#[derive(facet::Facet)]
#[facet(pod)]
pub struct ModelEntry {
    pub label: SharedString,
    pub path: String,
    #[facet(opaque)]
    entity: Arc<OnceLock<Entity>>,
    position_binding: Option<metor_proto::types::ComponentId>,
    orientation_binding: Option<metor_proto::types::ComponentId>,
    #[facet(opaque)]
    binding_tasks: SmallVec<[gpui::Task<()>; 2]>,
}

impl ModelEntry {
    pub fn position_binding_component(&self) -> Option<metor_proto::types::ComponentId> {
        self.position_binding
    }

    pub fn orientation_binding_component(&self) -> Option<metor_proto::types::ComponentId> {
        self.orientation_binding
    }

    fn position_orientation_entity(
        &self,
    ) -> (
        Option<metor_proto::types::ComponentId>,
        Option<metor_proto::types::ComponentId>,
        Arc<OnceLock<Entity>>,
    ) {
        (
            self.position_binding,
            self.orientation_binding,
            self.entity.clone(),
        )
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
        // entity cell is filled.
        let entities_for_release = entities.clone();
        cx.on_release(move |this, cx| {
            let ents = entities_for_release;
            let model_cells: Vec<Arc<OnceLock<Entity>>> =
                this.models.iter().map(|m| m.read(cx).entity.clone()).collect();
            cx.update_global::<BevyBridge, _>(|bridge, _| {
                for frame in this.current_frame.take().into_iter().chain(this.pending_release.take()) {
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

    /// Queue a world op and mark the viewer dirty. Every viewer-side
    /// mutator goes through this helper — it is the single point where
    /// `update_global::<BevyBridge, _>(…bridge.with_world(…))` appears
    /// in `Viewer3d`.
    fn with_world(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut World) + Send + 'static,
    ) {
        cx.update_global::<BevyBridge, _>(|bridge, _| bridge.with_world(f));
        self.mark_dirty(cx);
    }

    /// Queue a world op against one of this viewer's own entities. Thin
    /// wrapper over [`Self::with_world`] that loads the entity cell for
    /// the caller — the closure only runs if the initial spawn op has
    /// populated the cell, which FIFO op ordering guarantees for every
    /// mutation issued after construction.
    fn with_entities(
        &mut self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut World, ViewerEntities) + Send + 'static,
    ) {
        let cell = self.entities.clone();
        self.with_world(cx, move |world| {
            if let Some(&ents) = cell.get() {
                f(world, ents);
            }
        });
    }

    /// Queue a world op against a single model's entity, loading the
    /// cell the same way [`Self::with_entities`] does for viewer
    /// entities.
    fn with_model_entity(
        &mut self,
        cx: &mut Context<Self>,
        cell: Arc<OnceLock<Entity>>,
        f: impl FnOnce(&mut World, Entity) + Send + 'static,
    ) {
        self.with_world(cx, move |world| {
            if let Some(e) = cell.get().copied() {
                f(world, e);
            }
        });
    }

    /// Add a new model to the viewer. If `path` is non-empty the GLB is
    /// loaded immediately; otherwise the model is created with no asset
    /// and the user can fill the path in later via the inspector.
    pub fn add_model(
        &mut self,
        label: impl Into<SharedString>,
        path: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        let path = path.into();
        let entity: Arc<OnceLock<Entity>> = Arc::new(OnceLock::new());
        self.models.push(cx.new(|_| ModelEntry {
            label: label.into(),
            path: path.clone(),
            entity: entity.clone(),
            position_binding: None,
            orientation_binding: None,
            binding_tasks: SmallVec::new(),
        }));
        if !path.is_empty() {
            self.loading_until = Some(Instant::now() + POST_LOAD_WINDOW);
        }
        let layer = self.render_layer;
        self.with_world(cx, move |world| {
            let _ = entity.set(spawn_model(world, layer, &path));
        });
    }

    /// Remove one model from the viewer by its index in [`Self::models`].
    pub fn remove_model(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.models.len() {
            return;
        }
        let entry = self.models.remove(index);
        let entity_cell = entry.read(cx).entity.clone();
        self.with_model_entity(cx, entity_cell, |world, entity| {
            world.despawn(entity);
        });
    }

    /// Update one model's GLTF path and reload. We can't reuse the
    /// existing `OnceLock` cell (it's already set), so we mint a fresh
    /// cell for the new scene entity, swap it into the entry, and queue
    /// a despawn-then-spawn op.
    pub fn set_model_path(&mut self, index: usize, path: String, cx: &mut Context<Self>) {
        let Some(entry) = self.models.get(index).cloned() else {
            return;
        };
        let old_cell = entry.update(cx, |model, _| {
            model.path = path.clone();
            std::mem::replace(&mut model.entity, Arc::new(OnceLock::new()))
        });
        let new_cell = entry.read(cx).entity.clone();
        if !path.is_empty() {
            self.loading_until = Some(Instant::now() + POST_LOAD_WINDOW);
        }
        let layer = self.render_layer;
        self.with_world(cx, move |world| {
            if let Some(prev) = old_cell.get().copied() {
                world.despawn(prev);
            }
            let _ = new_cell.set(spawn_model(world, layer, &path));
        });
    }

    /// Update one model's display label. Inspector list row only.
    pub fn set_model_label(&mut self, index: usize, label: impl Into<SharedString>, cx: &mut Context<Self>) {
        let label = label.into();
        if let Some(entry) = self.models.get(index) {
            entry.update(cx, |model, _| {
                model.label = label;
            });
        }
    }

    /// Bind one model's position to a component.
    fn set_model_position_binding(
        &mut self,
        index: usize,
        component_id: metor_proto::types::ComponentId,
        cx: &mut Context<Self>,
    ) {
        if let Some(entry) = self.models.get(index).cloned() {
            entry.update(cx, |model, _| {
                model.position_binding = Some(component_id);
            });
        }
        self.restart_model_bindings(index, cx);
    }

    /// Bind one model's orientation to a component.
    fn set_model_orientation_binding(
        &mut self,
        index: usize,
        component_id: metor_proto::types::ComponentId,
        cx: &mut Context<Self>,
    ) {
        if let Some(entry) = self.models.get(index).cloned() {
            entry.update(cx, |model, _| {
                model.orientation_binding = Some(component_id);
            });
        }
        self.restart_model_bindings(index, cx);
    }

    /// Drop the streaming tasks for one model and respawn them from the
    /// current bindings, also resetting the world-side [`LiveDelta`]
    /// back to identity so a stale axis from a prior binding doesn't
    /// bleed into the next live transform write.
    fn restart_model_bindings(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(db) = self.db.clone() else {
            return;
        };
        let Some(entry) = self.models.get(index).cloned() else {
            return;
        };
        let (position, orientation, entity_cell) = entry.read(cx).position_orientation_entity();
        entry.update(cx, |model, _| {
            model.binding_tasks.clear();
        });

        self.with_model_entity(cx, entity_cell.clone(), |world, entity| {
            world.entity_mut(entity).insert(LiveDelta::default());
        });

        let mut tasks: SmallVec<[gpui::Task<()>; 2]> = SmallVec::new();
        if let Some(component_id) = position {
            tasks.push(Self::spawn_binding_stream(
                db.clone(),
                entity_cell.clone(),
                component_id,
                cx,
                pick_position,
                |v, delta| delta.translation = Some(v),
            ));
        }
        if let Some(component_id) = orientation {
            tasks.push(Self::spawn_binding_stream(
                db,
                entity_cell,
                component_id,
                cx,
                pick_attitude,
                |q, delta| delta.rotation = Some(q),
            ));
        }
        if let Some(entry) = self.models.get(index) {
            entry.update(cx, |model, _| {
                model.binding_tasks = tasks;
            });
        }
        self.mark_dirty(cx);
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
        component_id: metor_proto::types::ComponentId,
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
                    this.with_world(cx, move |world| {
                        let Some(entity) = cell.get().copied() else {
                            return;
                        };
                        let mut delta =
                            world.get::<LiveDelta>(entity).copied().unwrap_or_default();
                        apply(value, &mut delta);
                        world.entity_mut(entity).insert(delta);
                    });
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

                                    let mut releases = cx
                                        .update_global::<BevyBridge, _>(|bridge, _| {
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

// Inspectable field-id layout.
//
// Parent fields use IDs in 0..99. Per-model sub-field IDs are encoded as
// `1000 + item_index * 100 + sub_field_id`. The encoding is local to this
// element — `set_field` decodes it before routing.


