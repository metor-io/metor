//! 3D scene viewer element. A single Bevy `App` lives inside
//! [`bridge::BevyBridge`] (a `gpui::Global`) on the GPUI thread. Mutations
//! are direct ECS calls; renders ship the App to a background-executor
//! task and return the rendered frames to per-viewer slots on the bridge.
//!
//! Each `Viewer3d` is a thin GPUI element: it owns a [`ViewerId`], a list
//! of [`ModelEntry`]s with their streaming tasks, and a canvas that blits
//! whatever frame the bridge currently has stored for its id.

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

pub use bridge::{BevyBridge, ModelId, ViewerId};
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

/// Default render target size used the first time a viewer is created,
/// before the first canvas resize callback fires.
const INITIAL_SIZE: (u32, u32) = (512, 512);

/// A 3D scene element. Owns a list of models and a stable [`ViewerId`]
/// the bridge uses to look up its frame and ECS entities.
pub struct Viewer3d {
    viewer_id: ViewerId,
    /// DB handle so per-model bindings can start streams from the
    /// inspector. `None` when the viewer was constructed without a DB
    /// (the standalone example case).
    db: Option<Arc<DB>>,
    camera: OrbitCamera,
    /// All models currently displayed in this viewer. Order is the
    /// inspector's display order; lookups go by `ModelId`.
    models: Vec<ModelEntry>,
    /// Counter for minting fresh per-viewer [`ModelId`]s. Never reused.
    next_model_id: u64,
    /// Most-recently-rendered size sent to the bridge, used to debounce
    /// resize calls.
    requested_size: (u32, u32),
    drag: Option<DragState>,
}

/// One model in a viewer. Owns its path, label, bindings, the latest
/// live delta produced by its streaming tasks, and the tasks themselves.
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
    /// the full live transform (translation + rotation) to Bevy and don't
    /// lose the other axis.
    live: LiveTransform,
    /// One streaming task per active binding. Dropping the entry drops
    /// the tasks.
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
    /// Register this viewer with the process-wide Bevy bridge. Without a
    /// DB, the viewer can still load models via [`Self::add_model`] but
    /// can't install component bindings.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self::new_inner(None, cx)
    }

    /// Create a viewer connected to `db` so the inspector can install
    /// position and orientation bindings from its components.
    pub fn with_db(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        Self::new_inner(Some(db), cx)
    }

    fn new_inner(db: Option<Arc<DB>>, cx: &mut Context<Self>) -> Self {
        let weak = cx.entity().downgrade();
        let viewer_id = {
            let bridge = BevyBridge::get_or_init(cx);
            bridge.register(weak, INITIAL_SIZE.0, INITIAL_SIZE.1)
        };

        // Register the on_release callback so the bridge cleans up when
        // the entity goes away.
        cx.on_release(move |this, cx| {
            cx.update_global::<BevyBridge, _>(|bridge, _| {
                bridge.unregister(this.viewer_id);
            });
        })
        .detach();

        let camera = OrbitCamera::default();
        let viewer = Self {
            viewer_id,
            db,
            camera,
            models: Vec::new(),
            next_model_id: 1,
            requested_size: INITIAL_SIZE,
            drag: None,
        };
        viewer.sync_camera(cx);
        viewer
    }

    /// Borrow the current model list. Used by serialization and the
    /// inspector to walk all models.
    pub fn models(&self) -> &[ModelEntry] {
        &self.models
    }

    pub fn camera(&self) -> OrbitCamera {
        self.camera
    }

    /// Add a new model to the viewer. If `path` is non-empty the GLB is
    /// loaded immediately; otherwise the model is created with no asset
    /// (the user can fill in a path later via the inspector).
    pub fn add_model(
        &mut self,
        label: impl Into<SharedString>,
        path: impl Into<String>,
        cx: &mut Context<Self>,
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
            let viewer_id = self.viewer_id;
            cx.update_global::<BevyBridge, _>(|bridge, _| {
                bridge.load_model(viewer_id, id, path.into());
            });
        }
        id
    }

    /// Despawn one model from the viewer. Drops the entry's binding
    /// tasks (terminating the streams) and tells Bevy to despawn the
    /// entity.
    pub fn remove_model(&mut self, model_id: ModelId, cx: &mut Context<Self>) {
        let Some(idx) = self.models.iter().position(|m| m.id == model_id) else {
            return;
        };
        self.models.remove(idx);
        let viewer_id = self.viewer_id;
        cx.update_global::<BevyBridge, _>(|bridge, _| {
            bridge.remove_model(viewer_id, model_id);
        });
    }

    /// Update one model's GLTF path. Tells Bevy to load the new GLB if
    /// the path is non-empty.
    pub fn set_model_path(&mut self, model_id: ModelId, path: String, cx: &mut Context<Self>) {
        let Some(entry) = self.models.iter_mut().find(|m| m.id == model_id) else {
            return;
        };
        entry.path = path.clone();
        if !path.is_empty() {
            let viewer_id = self.viewer_id;
            cx.update_global::<BevyBridge, _>(|bridge, _| {
                bridge.load_model(viewer_id, model_id, path.into());
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
        // Reset the cached live delta so a stale axis from a prior
        // binding doesn't bleed into the next live transform send.
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

    /// Round a pixel size to a 32-pixel grid. Avoids a `Resize` call on
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

    /// If `new_size` differs from the last-requested size, ask the
    /// bridge to resize this viewer's render target.
    fn maybe_resize(&mut self, new_size: (u32, u32), cx: &mut Context<Self>) {
        if self.requested_size == new_size {
            return;
        }
        self.requested_size = new_size;
        let viewer_id = self.viewer_id;
        cx.update_global::<BevyBridge, _>(|bridge, _| {
            bridge.resize(viewer_id, new_size.0, new_size.1);
        });
    }

    /// Push the current camera pose to the bridge.
    fn sync_camera(&self, cx: &mut Context<Self>) {
        let viewer_id = self.viewer_id;
        let cam = self.camera;
        cx.update_global::<BevyBridge, _>(|bridge, _| {
            bridge.set_camera(
                viewer_id,
                cam.target,
                cam.yaw,
                cam.pitch,
                cam.distance,
                cam.fov_y_rad,
            );
        });
    }

    /// Reset the camera to its default pose.
    fn reset_camera(&mut self, cx: &mut Context<Self>) {
        self.camera = OrbitCamera::default();
        self.sync_camera(cx);
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
                let result = this.update(cx, |this, cx| {
                    let Some(entry) = this.models.iter_mut().find(|m| m.id == model_id) else {
                        return false;
                    };
                    entry.live.translation = Some(v);
                    let translation = entry.live.translation;
                    let rotation = entry.live.rotation;
                    let viewer_id = this.viewer_id;
                    cx.update_global::<BevyBridge, _>(|bridge, _| {
                        bridge.set_live_transform(viewer_id, model_id, translation, rotation);
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
                let result = this.update(cx, |this, cx| {
                    let Some(entry) = this.models.iter_mut().find(|m| m.id == model_id) else {
                        return false;
                    };
                    entry.live.rotation = Some(q);
                    let translation = entry.live.translation;
                    let rotation = entry.live.rotation;
                    let viewer_id = this.viewer_id;
                    cx.update_global::<BevyBridge, _>(|bridge, _| {
                        bridge.set_live_transform(viewer_id, model_id, translation, rotation);
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
}

/// Wrap tight BGRA bytes into a `gpui::RenderImage` suitable for
/// `Window::paint_image`. Used by the bridge's render task on the
/// background worker.
pub(crate) fn make_render_image_from_bytes(
    width: u32,
    height: u32,
    rgba: Vec<u8>,
) -> Option<Arc<RenderImage>> {
    let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(width, height, rgba)?;
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
                    cx.notify();
                }),
            )
            .on_scroll_wheel(
                cx.listener(|this, event: &gpui::ScrollWheelEvent, _window, cx| {
                    let delta = event.delta.pixel_delta(px(20.0));
                    this.camera.zoom(-f32::from(delta.y));
                    this.sync_camera(cx);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(
                canvas(
                    {
                        // Prepaint: read the current frame for this
                        // viewer from the bridge, debounce-resize, and
                        // schedule a Bevy render so the next paint cycle
                        // sees a fresher frame.
                        let this = cx.entity().downgrade();
                        move |bounds: Bounds<Pixels>, window, cx| {
                            let scale = window.scale_factor();
                            let new_size = Viewer3d::quantize(bounds.size, scale);
                            let frame = this
                                .update(cx, |this, cx| {
                                    this.maybe_resize(new_size, cx);
                                    let id = this.viewer_id;
                                    cx.global::<BevyBridge>().frame_for(id)
                                })
                                .unwrap_or(None);
                            BevyBridge::schedule_render(cx);
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
                        self.set_model_path(model_id, s, cx);
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
                    self.remove_model(model_id, cx);
                }
                _ => {}
            }
            cx.notify();
            return;
        }

        match field_id.0 {
            FIELD_MODELS => {
                // List value is never written to directly — sub-fields
                // handle all mutations.
            }
            FIELD_ADD_MODEL => {
                self.add_model("", "", cx);
            }
            FIELD_FOV => {
                if let InspectionValue::F64(v) = value {
                    self.camera.fov_y_rad = (v as f32).max(0.01);
                    self.sync_camera(cx);
                }
            }
            FIELD_RESET_CAMERA => {
                self.reset_camera(cx);
            }
            _ => {}
        }
        cx.notify();
    }
}
