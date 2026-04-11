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
    Bounds, Context, Corners, IntoElement, MouseButton, Pixels, Point, RenderImage, Window, canvas,
    div, prelude::*, px,
};
use image::{Frame, ImageBuffer, Rgba};
use metor_db::DB;
use metor_proto::types::ComponentView;
use smallvec::SmallVec;

use crate::inspectable::{FieldId, Inspectable, InspectionField, InspectionValue, PickerArity};
use crate::theme::theme;
use crate::{AsComponentView, ComponentStream, ComponentStreamBuilder};

pub use bridge::{BevyBridge, ViewerCommand, ViewerFrame, ViewerHandle, ViewerId};
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
    /// DB handle so `bind_position` / `bind_orientation` can start streams
    /// from the inspector. `None` when the viewer was constructed without a
    /// DB (the standalone example case).
    db: Option<Arc<DB>>,
    camera: OrbitCamera,
    /// GLTF/GLB path currently loaded. Empty string means "use the default
    /// placeholder cube".
    model_path: String,
    /// Latest position binding configuration, if any.
    position_binding: Option<Binding>,
    /// Latest orientation binding configuration, if any.
    orientation_binding: Option<Binding>,
    frame_image: Option<Arc<RenderImage>>,
    frame_size: (u32, u32),
    /// The size most recently requested from the Bevy side. Tracked
    /// separately from `frame_size` (which is the size of the *last frame
    /// received*) so we don't re-issue `Resize` commands for every paint.
    requested_size: (u32, u32),
    dropped_images: Vec<Arc<RenderImage>>,
    drag: Option<DragState>,
    /// Latest live delta aggregated from the binding streaming tasks.
    /// Kept so that re-issuing a SetLiveTransform (e.g. after a LoadModel)
    /// doesn't forget the currently-bound axes.
    live: LiveTransform,
    /// One task per binding. Dropping the viewer drops the tasks, which
    /// terminate the associated streams.
    binding_tasks: SmallVec<[gpui::Task<()>; 2]>,
    _pump: gpui::Task<()>,
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
            model_path: String::new(),
            position_binding: None,
            orientation_binding: None,
            frame_image: None,
            frame_size: INITIAL_SIZE,
            requested_size: INITIAL_SIZE,
            dropped_images: Vec::new(),
            drag: None,
            live: LiveTransform::default(),
            binding_tasks: SmallVec::new(),
            _pump: pump,
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

    /// Request that the viewer's current model be replaced with a GLTF/GLB
    /// loaded from `path`. The path is resolved by Bevy's `AssetServer`, so
    /// an absolute path works anywhere; a relative path resolves against the
    /// asset source, which defaults to the process working directory.
    pub fn load_gltf(&self, path: impl Into<std::path::PathBuf>) {
        self.handle.send(ViewerCommand::LoadModel {
            id: self.handle.id(),
            path: path.into(),
        });
    }

    pub fn camera(&self) -> OrbitCamera {
        self.camera
    }

    pub fn model_path(&self) -> &str {
        &self.model_path
    }

    /// JSON snapshot of the position binding for serialization. Returns
    /// `Null` when no binding is set.
    pub fn position_binding_serialized(&self) -> serde_json::Value {
        match self.position_binding {
            Some(b) => serde_json::json!({
                "component_id": format!("{:?}", b.component_id),
            }),
            None => serde_json::Value::Null,
        }
    }

    /// JSON snapshot of the orientation binding for serialization. Returns
    /// `Null` when no binding is set.
    pub fn orientation_binding_serialized(&self) -> serde_json::Value {
        match self.orientation_binding {
            Some(b) => serde_json::json!({
                "component_id": format!("{:?}", b.component_id),
            }),
            None => serde_json::Value::Null,
        }
    }

    /// Rebuild the binding streaming tasks from `self.position_binding` /
    /// `self.orientation_binding`. Called whenever the inspector changes
    /// either binding (or when the DB becomes available).
    fn restart_bindings(&mut self, cx: &mut Context<Self>) {
        let Some(db) = self.db.clone() else {
            return;
        };
        self.binding_tasks.clear();
        if let Some(b) = self.position_binding {
            self.binding_tasks
                .push(Self::spawn_position_task(db.clone(), b.component_id, cx));
        }
        if let Some(b) = self.orientation_binding {
            self.binding_tasks
                .push(Self::spawn_orientation_task(db, b.component_id, cx));
        }
    }

    fn spawn_position_task(
        db: Arc<DB>,
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
                if this
                    .update(cx, |this, _cx| {
                        this.live.translation = Some(v);
                        this.handle.send(ViewerCommand::SetLiveTransform {
                            id: this.handle.id(),
                            translation: this.live.translation,
                            rotation: this.live.rotation,
                        });
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
    }

    fn spawn_orientation_task(
        db: Arc<DB>,
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
                if this
                    .update(cx, |this, _cx| {
                        this.live.rotation = Some(q);
                        this.handle.send(ViewerCommand::SetLiveTransform {
                            id: this.handle.id(),
                            translation: this.live.translation,
                            rotation: this.live.rotation,
                        });
                    })
                    .is_err()
                {
                    break;
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

// Inspectable field-id layout. Kept as constants so the mapping is obvious
// from the `set_field` match arms below.
const FIELD_MODEL_PATH: u32 = 0;
const FIELD_POSITION: u32 = 1;
const FIELD_ORIENTATION: u32 = 2;
const FIELD_FOV: u32 = 3;
const FIELD_RESET_CAMERA: u32 = 4;

fn binding_to_value(binding: Option<Binding>, arity: PickerArity) -> InspectionValue {
    InspectionValue::ElementPicker {
        component: binding.map(|b| b.component_id),
        arity,
    }
}

impl Inspectable for Viewer3d {
    fn fields(&self) -> Vec<InspectionField> {
        vec![
            InspectionField {
                label: "Model Path".into(),
                field_id: FieldId(FIELD_MODEL_PATH),
                value: InspectionValue::String(self.model_path.clone()),
            },
            InspectionField {
                label: "Position".into(),
                field_id: FieldId(FIELD_POSITION),
                value: binding_to_value(self.position_binding, PickerArity::Vec3),
            },
            InspectionField {
                label: "Orientation".into(),
                field_id: FieldId(FIELD_ORIENTATION),
                value: binding_to_value(self.orientation_binding, PickerArity::Quat),
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
        match field_id.0 {
            FIELD_MODEL_PATH => {
                if let InspectionValue::String(s) = value {
                    self.model_path = s.clone();
                    if !s.is_empty() {
                        self.handle.send(ViewerCommand::LoadModel {
                            id: self.handle.id(),
                            path: s.into(),
                        });
                    }
                }
            }
            FIELD_POSITION => {
                if let InspectionValue::ElementPicker {
                    component: Some(component_id),
                    ..
                } = value
                {
                    self.position_binding = Some(Binding { component_id });
                    self.restart_bindings(cx);
                }
            }
            FIELD_ORIENTATION => {
                if let InspectionValue::ElementPicker {
                    component: Some(component_id),
                    ..
                } = value
                {
                    self.orientation_binding = Some(Binding { component_id });
                    self.restart_bindings(cx);
                }
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
