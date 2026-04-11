//! 3D scene viewer element. A single Bevy `App` runs on a dedicated thread
//! and is shared by every [`Viewer3d`] instance; communication is via
//! crossbeam channels managed by [`bridge::BevyBridge`].
//!
//! Each `Viewer3d` holds a [`ViewerHandle`], a pumping task that drains the
//! frame channel into an `Arc<RenderImage>`, and a canvas paint callback that
//! blits that image the same way the time-series plot does.

pub(crate) mod bevy_app;
pub mod bridge;
pub mod camera;

use std::sync::Arc;

use glam::{Quat, Vec3};
use gpui::{
    Bounds, Context, Corners, IntoElement, MouseButton, Pixels, Point, RenderImage, SharedString,
    Window, canvas, div, prelude::*, px,
};
use image::{Frame, ImageBuffer, Rgba};
use metor_db::DB;
use metor_proto::types::ComponentView;
use smallvec::SmallVec;

use crate::inspectable::{
    FieldId, Inspectable, InspectionField, InspectionValue, ListItem, PickerArity,
};
use crate::theme::theme;
use crate::{AsComponentView, ComponentStream, ComponentStreamBuilder};

pub use bridge::{BevyBridge, ModelId, ViewerCommand, ViewerFrame, ViewerHandle, ViewerId};
pub use camera::OrbitCamera;

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

/// Default render target size used the first time a viewer is created, before
/// the first resize callback fires. Small enough to not waste VRAM, large
/// enough that the first frame isn't a pixelated mess.
const INITIAL_SIZE: (u32, u32) = (512, 512);

/// A single 3D scene embedded in a GPUI canvas. Frames come from the shared
/// Bevy app via [`ViewerHandle::frame_rx`]; rendering is handled by a pump
/// task that converts them into `Arc<RenderImage>` and stores them on the
/// element for the next paint.
pub struct Viewer3d {
    handle: ViewerHandle,
    /// DB handle so per-model bindings can start streams from the
    /// inspector. `None` when the viewer was constructed without a DB (the
    /// standalone example case).
    db: Option<Arc<DB>>,
    camera: OrbitCamera,
    /// All models currently displayed in this viewer. Order is the
    /// inspector's display order; lookups go by `ModelId`.
    models: Vec<ModelEntry>,
    /// Counter for minting fresh per-viewer [`ModelId`]s. Never reused.
    next_model_id: u64,
    frame_image: Option<Arc<RenderImage>>,
    frame_size: (u32, u32),
    /// The size most recently requested from the Bevy side. Tracked
    /// separately from `frame_size` (which is the size of the *last frame
    /// received*) so we don't re-issue `Resize` commands for every paint.
    requested_size: (u32, u32),
    dropped_images: Vec<Arc<RenderImage>>,
    drag: Option<DragState>,
    _pump: gpui::Task<()>,
}

/// One model in a viewer. Owns its path, label, bindings, the latest live
/// delta produced by its streaming tasks, and the tasks themselves.
/// Dropping a [`ModelEntry`] drops its tasks, which terminates the
/// associated WAL streams cleanly.
pub struct ModelEntry {
    pub id: ModelId,
    pub label: SharedString,
    pub path: String,
    position_binding: Option<Binding>,
    orientation_binding: Option<Binding>,
    /// Latest live delta produced by the streaming tasks for this model.
    /// Kept on the GPUI side so that when one binding updates, we re-emit
    /// the full `SetLiveTransform` (translation + rotation) and don't lose
    /// the other axis.
    live: LiveTransform,
    /// One streaming task per active binding. Dropping the entry drops the
    /// tasks.
    binding_tasks: SmallVec<[gpui::Task<()>; 2]>,
}

impl ModelEntry {
    pub fn position_binding_component(&self) -> Option<metor_proto::types::ComponentId> {
        self.position_binding.map(|b| b.component_id)
    }

    pub fn orientation_binding_component(&self) -> Option<metor_proto::types::ComponentId> {
        self.orientation_binding.map(|b| b.component_id)
    }
}

#[derive(Clone, Copy, Debug)]
struct Binding {
    component_id: metor_proto::types::ComponentId,
}

/// GPUI-side cache of the live delta so that multiple bindings can update
/// translation and rotation independently without racing.
#[derive(Default, Clone, Copy)]
struct LiveTransform {
    translation: Option<glam::Vec3>,
    rotation: Option<glam::Quat>,
}

/// State captured at the start of a drag so the delta is applied to the
/// camera's pre-drag pose — avoids drift from accumulating pixel deltas frame
/// by frame.
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
    /// Register this viewer with the process-wide Bevy bridge and spawn the
    /// frame-pump task. Without a DB, the viewer can still render models
    /// loaded via [`Self::load_gltf`] but can't install component bindings.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self::new_inner(None, cx)
    }

    /// Create a viewer connected to `db` so the inspector can install
    /// position and orientation bindings from its components.
    pub fn with_db(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        Self::new_inner(Some(db), cx)
    }

    fn new_inner(db: Option<Arc<DB>>, cx: &mut Context<Self>) -> Self {
        let handle = {
            let bridge = BevyBridge::get_or_init(cx);
            bridge.register(INITIAL_SIZE.0, INITIAL_SIZE.1)
        };
        let camera = OrbitCamera::default();
        handle.send(camera.to_command(handle.id()));
        let pump = Self::spawn_pump(&handle, cx);
        Self {
            handle,
            db,
            camera,
            models: Vec::new(),
            next_model_id: 1,
            frame_image: None,
            frame_size: INITIAL_SIZE,
            requested_size: INITIAL_SIZE,
            dropped_images: Vec::new(),
            drag: None,
            _pump: pump,
        }
    }

    /// Borrow the current model list. Used by serialization and the
    /// inspector to walk all models.
    pub fn models(&self) -> &[ModelEntry] {
        &self.models
    }

    /// Add a new model to the viewer. If `path` is non-empty the GLB is
    /// loaded immediately; otherwise the model is created with no asset
    /// (the user can fill in a path later via the inspector).
    pub fn add_model(
        &mut self,
        label: impl Into<SharedString>,
        path: impl Into<String>,
    ) -> ModelId {
        let id = ModelId(self.next_model_id);
        self.next_model_id += 1;
        let path = path.into();
        let entry = ModelEntry {
            id,
            label: label.into(),
            path: path.clone(),
            position_binding: None,
            orientation_binding: None,
            live: LiveTransform::default(),
            binding_tasks: SmallVec::new(),
        };
        self.models.push(entry);
        if !path.is_empty() {
            self.handle.send(ViewerCommand::LoadModel {
                id: self.handle.id(),
                model_id: id,
                path: path.into(),
            });
        }
        id
    }

    /// Despawn one model from the viewer. Drops the entry's binding tasks
    /// (terminating the streams) and tells Bevy to despawn the entity.
    pub fn remove_model(&mut self, model_id: ModelId) {
        let Some(idx) = self.models.iter().position(|m| m.id == model_id) else {
            return;
        };
        // Drop the entry first so its `binding_tasks` are released — any
        // in-flight stream value the task was about to publish will be
        // discarded rather than racing the RemoveModel command.
        self.models.remove(idx);
        self.handle.send(ViewerCommand::RemoveModel {
            id: self.handle.id(),
            model_id,
        });
    }

    /// Update one model's GLTF path. Sends a `LoadModel` to Bevy if the
    /// new path is non-empty.
    pub fn set_model_path(&mut self, model_id: ModelId, path: String) {
        let Some(entry) = self.models.iter_mut().find(|m| m.id == model_id) else {
            return;
        };
        entry.path = path.clone();
        if !path.is_empty() {
            self.handle.send(ViewerCommand::LoadModel {
                id: self.handle.id(),
                model_id,
                path: path.into(),
            });
        }
    }

    /// Update one model's display label. Affects the inspector list row
    /// only — does not touch the Bevy side.
    pub fn set_model_label(&mut self, model_id: ModelId, label: impl Into<SharedString>) {
        if let Some(entry) = self.models.iter_mut().find(|m| m.id == model_id) {
            entry.label = label.into();
        }
    }

    /// Bind one model's position to a component. Tears down and respawns
    /// only that model's streaming tasks.
    pub fn set_model_position_binding(
        &mut self,
        model_id: ModelId,
        component_id: metor_proto::types::ComponentId,
        cx: &mut Context<Self>,
    ) {
        if let Some(entry) = self.models.iter_mut().find(|m| m.id == model_id) {
            entry.position_binding = Some(Binding { component_id });
        }
        self.restart_model_bindings(model_id, cx);
    }

    /// Bind one model's orientation to a component. Tears down and
    /// respawns only that model's streaming tasks.
    pub fn set_model_orientation_binding(
        &mut self,
        model_id: ModelId,
        component_id: metor_proto::types::ComponentId,
        cx: &mut Context<Self>,
    ) {
        if let Some(entry) = self.models.iter_mut().find(|m| m.id == model_id) {
            entry.orientation_binding = Some(Binding { component_id });
        }
        self.restart_model_bindings(model_id, cx);
    }

    /// Drop the streaming tasks for one model and respawn them from its
    /// current bindings.
    fn restart_model_bindings(&mut self, model_id: ModelId, cx: &mut Context<Self>) {
        let Some(db) = self.db.clone() else {
            return;
        };
        let Some(entry) = self.models.iter_mut().find(|m| m.id == model_id) else {
            return;
        };
        let position = entry.position_binding;
        let orientation = entry.orientation_binding;
        entry.binding_tasks.clear();
        // Reset the cached live delta so a stale axis from a prior binding
        // doesn't bleed into the next SetLiveTransform.
        entry.live = LiveTransform::default();

        let mut tasks: SmallVec<[gpui::Task<()>; 2]> = SmallVec::new();
        if let Some(b) = position {
            tasks.push(Self::spawn_position_task(
                db.clone(),
                model_id,
                b.component_id,
                cx,
            ));
        }
        if let Some(b) = orientation {
            tasks.push(Self::spawn_orientation_task(db, model_id, b.component_id, cx));
        }
        if let Some(entry) = self.models.iter_mut().find(|m| m.id == model_id) {
            entry.binding_tasks = tasks;
        }
    }

    /// Round a pixel size to a 32-pixel grid. Avoids a `Resize` command on
    /// every single pixel the user drags a window edge by, while still
    /// adapting quickly enough that the image doesn't stretch visibly.
    fn quantize(size: gpui::Size<Pixels>, scale: f32) -> (u32, u32) {
        const STEP: u32 = 32;
        const MIN: u32 = 64;
        let phys_w = (f32::from(size.width) * scale).max(1.0) as u32;
        let phys_h = (f32::from(size.height) * scale).max(1.0) as u32;
        let round = |v: u32| v.div_ceil(STEP) * STEP;
        (round(phys_w).max(MIN), round(phys_h).max(MIN))
    }

    /// If `new_size` differs from the last-requested size, send a `Resize`
    /// command to the Bevy world and remember it.
    fn maybe_resize(&mut self, new_size: (u32, u32)) {
        if self.requested_size == new_size {
            return;
        }
        self.requested_size = new_size;
        self.handle.send(ViewerCommand::Resize {
            id: self.handle.id(),
            width: new_size.0,
            height: new_size.1,
        });
    }

    /// Send the current camera state to the Bevy world. Called after every
    /// interaction that mutates `self.camera`.
    fn sync_camera(&self) {
        self.handle.send(self.camera.to_command(self.handle.id()));
    }

    /// Reset the camera to its default pose.
    fn reset_camera(&mut self) {
        self.camera = OrbitCamera::default();
        self.sync_camera();
    }

    pub fn camera(&self) -> OrbitCamera {
        self.camera
    }

    fn spawn_position_task(
        db: Arc<DB>,
        model_id: ModelId,
        component_id: metor_proto::types::ComponentId,
        cx: &mut Context<Self>,
    ) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            let mut stream = component_id.into_stream(&db).await;
            loop {
                let v = {
                    let view = stream.next().await;
                    pick_position(&view.as_component_view())
                };
                let result = this.update(cx, |this, _cx| {
                    let Some(entry) = this.models.iter_mut().find(|m| m.id == model_id) else {
                        // Model has been removed; signal the loop to break.
                        return false;
                    };
                    entry.live.translation = Some(v);
                    let translation = entry.live.translation;
                    let rotation = entry.live.rotation;
                    this.handle.send(ViewerCommand::SetLiveTransform {
                        id: this.handle.id(),
                        model_id,
                        translation,
                        rotation,
                    });
                    true
                });
                match result {
                    Ok(true) => {}
                    _ => break,
                }
            }
        })
    }

    fn spawn_orientation_task(
        db: Arc<DB>,
        model_id: ModelId,
        component_id: metor_proto::types::ComponentId,
        cx: &mut Context<Self>,
    ) -> gpui::Task<()> {
        cx.spawn(async move |this, cx| {
            let mut stream = component_id.into_stream(&db).await;
            loop {
                let q = {
                    let view = stream.next().await;
                    pick_attitude(&view.as_component_view())
                };
                let result = this.update(cx, |this, _cx| {
                    let Some(entry) = this.models.iter_mut().find(|m| m.id == model_id) else {
                        return false;
                    };
                    entry.live.rotation = Some(q);
                    let translation = entry.live.translation;
                    let rotation = entry.live.rotation;
                    this.handle.send(ViewerCommand::SetLiveTransform {
                        id: this.handle.id(),
                        model_id,
                        translation,
                        rotation,
                    });
                    true
                });
                match result {
                    Ok(true) => {}
                    _ => break,
                }
            }
        })
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
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _window, cx| {
                    if event.click_count >= 2 {
                        this.reset_camera();
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
                    this.sync_camera();
                    cx.notify();
                }),
            )
            .on_scroll_wheel(
                cx.listener(|this, event: &gpui::ScrollWheelEvent, _window, cx| {
                    let delta = event.delta.pixel_delta(px(20.0));
                    this.camera.zoom(-f32::from(delta.y));
                    this.sync_camera();
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(
                canvas(
                    // Prepaint: capture the latest image + dropped-image list,
                    // and issue a `Resize` command if the bounds have changed.
                    // Returning them as state makes the paint closure
                    // self-contained and lets us release old images by calling
                    // `window.drop_image` without touching element state.
                    {
                        let this = cx.entity().downgrade();
                        move |bounds: Bounds<Pixels>, window, cx| {
                            let scale = window.scale_factor();
                            let new_size = Viewer3d::quantize(bounds.size, scale);
                            let state = this
                                .update(cx, |this, _cx| {
                                    this.maybe_resize(new_size);
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

// Inspectable field-id layout.
//
// Parent fields use IDs in 0..99. Per-model sub-field IDs are encoded as
// `1000 + item_index * 100 + sub_field_id`. The encoding is local to this
// element — `set_field` decodes it before routing.

const FIELD_MODELS: u32 = 0;
const FIELD_ADD_MODEL: u32 = 1;
const FIELD_FOV: u32 = 2;
const FIELD_RESET_CAMERA: u32 = 3;

const SUB_LABEL: u32 = 0;
const SUB_PATH: u32 = 1;
const SUB_POSITION: u32 = 2;
const SUB_ORIENTATION: u32 = 3;
const SUB_REMOVE: u32 = 4;

fn encode_sub(item_index: usize, sub: u32) -> u32 {
    1000 + item_index as u32 * 100 + sub
}

fn decode_sub(field_id: u32) -> Option<(usize, u32)> {
    if field_id < 1000 {
        return None;
    }
    let raw = field_id - 1000;
    Some(((raw / 100) as usize, raw % 100))
}

fn binding_to_value(
    component: Option<metor_proto::types::ComponentId>,
    arity: PickerArity,
) -> InspectionValue {
    InspectionValue::ElementPicker { component, arity }
}

/// Pull a display label out of a model entry. Falls back to the path's
/// basename if the label is empty, then to a placeholder for empty
/// entries.
fn model_row_label(entry: &ModelEntry, index: usize) -> SharedString {
    if !entry.label.is_empty() {
        return entry.label.clone();
    }
    if !entry.path.is_empty() {
        if let Some(name) = std::path::Path::new(&entry.path)
            .file_name()
            .and_then(|n| n.to_str())
        {
            return SharedString::from(name.to_string());
        }
    }
    SharedString::from(format!("Model {}", index + 1))
}

impl Inspectable for Viewer3d {
    fn fields(&self) -> Vec<InspectionField> {
        let model_items: Vec<ListItem> = self
            .models
            .iter()
            .enumerate()
            .map(|(i, entry)| ListItem {
                label: model_row_label(entry, i),
                fields: vec![
                    InspectionField {
                        label: "Label".into(),
                        field_id: FieldId(encode_sub(i, SUB_LABEL)),
                        value: InspectionValue::String(entry.label.to_string()),
                    },
                    InspectionField {
                        label: "Path".into(),
                        field_id: FieldId(encode_sub(i, SUB_PATH)),
                        value: InspectionValue::String(entry.path.clone()),
                    },
                    InspectionField {
                        label: "Position".into(),
                        field_id: FieldId(encode_sub(i, SUB_POSITION)),
                        value: binding_to_value(
                            entry.position_binding_component(),
                            PickerArity::Vec3,
                        ),
                    },
                    InspectionField {
                        label: "Orientation".into(),
                        field_id: FieldId(encode_sub(i, SUB_ORIENTATION)),
                        value: binding_to_value(
                            entry.orientation_binding_component(),
                            PickerArity::Quat,
                        ),
                    },
                    InspectionField {
                        label: "Remove".into(),
                        field_id: FieldId(encode_sub(i, SUB_REMOVE)),
                        value: InspectionValue::Bool(false),
                    },
                ],
            })
            .collect();

        vec![
            InspectionField {
                label: "Models".into(),
                field_id: FieldId(FIELD_MODELS),
                value: InspectionValue::List(model_items),
            },
            InspectionField {
                label: "Add Model".into(),
                field_id: FieldId(FIELD_ADD_MODEL),
                value: InspectionValue::Bool(false),
            },
            InspectionField {
                label: "Camera FOV (rad)".into(),
                field_id: FieldId(FIELD_FOV),
                value: InspectionValue::F64(self.camera.fov_y_rad as f64),
            },
            InspectionField {
                label: "Reset Camera".into(),
                field_id: FieldId(FIELD_RESET_CAMERA),
                value: InspectionValue::Bool(false),
            },
        ]
    }

    fn set_field(&mut self, field_id: FieldId, value: InspectionValue, cx: &mut Context<Self>) {
        if let Some((item_index, sub)) = decode_sub(field_id.0) {
            // Resolve the model by index, capturing the stable ModelId so
            // any subsequent self-mutations don't depend on the borrow.
            let Some(model_id) = self.models.get(item_index).map(|m| m.id) else {
                return;
            };
            match sub {
                SUB_LABEL => {
                    if let InspectionValue::String(s) = value {
                        self.set_model_label(model_id, s);
                    }
                }
                SUB_PATH => {
                    if let InspectionValue::String(s) = value {
                        self.set_model_path(model_id, s);
                    }
                }
                SUB_POSITION => {
                    if let InspectionValue::ElementPicker {
                        component: Some(component_id),
                        ..
                    } = value
                    {
                        self.set_model_position_binding(model_id, component_id, cx);
                    }
                }
                SUB_ORIENTATION => {
                    if let InspectionValue::ElementPicker {
                        component: Some(component_id),
                        ..
                    } = value
                    {
                        self.set_model_orientation_binding(model_id, component_id, cx);
                    }
                }
                SUB_REMOVE => {
                    self.remove_model(model_id);
                }
                _ => {}
            }
            cx.notify();
            return;
        }

        match field_id.0 {
            FIELD_MODELS => {
                // The List value itself is never written to directly — its
                // sub-fields handle all mutations.
            }
            FIELD_ADD_MODEL => {
                // Bool toggle is a one-shot action: any value (true or
                // false) means "add an empty model now".
                self.add_model("", "");
            }
            FIELD_FOV => {
                if let InspectionValue::F64(v) = value {
                    self.camera.fov_y_rad = (v as f32).max(0.01);
                    self.sync_camera();
                }
            }
            FIELD_RESET_CAMERA => {
                self.reset_camera();
            }
            _ => {}
        }
        cx.notify();
    }
}
