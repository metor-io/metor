//! The image view: one image component at device resolution.

use std::sync::Arc;

use gpui::{
    Bounds, Context, Corners, Entity, FocusHandle, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, Point, Render, ScrollWheelEvent, SharedString,
    Subscription, Task, WeakEntity, Window, canvas, div, point, prelude::*, px, size,
};
use metor_db::DB;
use metor_proto::types::ComponentId;
use serde::{Deserialize, Serialize};

use super::histogram::Histogram;
use super::layout::FrameLayout;
use super::lut::{Display, Levels, Lut};
use super::overlay::{self, Overlay, OverlayColor, OverlayConfig, Points};
use super::readout::{Readout, Siblings, raw_value};
use super::render::{self, Frames, Job, LutKey, RenderKey, Rendered, Scheduler};
use super::resample::{MAX_STEP, MIN_STEP, Rect, RenderError, Scratch, Viewport};
use super::source::{Frame, FrameSource, FrameState, RecordSource};
use super::strip::{self, HistogramStrip, StripData};
use crate::data_binding::Binding;
use crate::theme::{theme, ui_px, ui_scale};
use crate::views::popover;
use crate::views::time_series::cursor::{estimate_readout_size, readout_origin};

/// Zoom per wheel notch.
const ZOOM_PER_NOTCH: f64 = 1.25;
/// Logical pixels of wheel travel per notch.
const NOTCH_PX: f32 = 20.0;
/// Device pixels of the image a pan always leaves in view.
const PAN_MARGIN: f64 = 32.0;

/// How the frame is placed in the view.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    /// The whole frame, centred.
    #[default]
    Fit,
    /// `center` is the source point at the middle of the view; `step` is
    /// source pixels per device pixel.
    Free { center: (f64, f64), step: f64 },
}

/// Persisted shape of an [`ImageView`], shared by the tile and dashboard.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ImageViewConfig {
    pub component: String,
    /// `None` until set: a fresh view takes the producer's hints.
    pub display: Option<Display>,
    pub view: ViewMode,
    /// Shows the histogram strip under the image as well as in the
    /// inspector. Layouts from before the inspector histogram saved this as
    /// `histogram`, always on by default, so that key is ignored.
    pub histogram_overlay: bool,
    /// Shows a crosshair at the cursor.
    pub crosshair: bool,
    pub overlays: Vec<OverlayConfig>,
}

impl Default for ImageViewConfig {
    fn default() -> Self {
        Self {
            component: String::new(),
            display: None,
            view: ViewMode::Fit,
            histogram_overlay: false,
            crosshair: false,
            overlays: Vec::new(),
        }
    }
}

/// The config blob of an image view on `component`.
pub(crate) fn image_view_config(component: String) -> String {
    let cfg = ImageViewConfig {
        component,
        ..Default::default()
    };
    serde_json::to_string(&cfg).unwrap_or_default()
}

#[derive(facet::Facet)]
pub struct ImageView {
    /// The image component, picked through the inspector's image list.
    #[facet(skip)]
    pub source: Binding,
    pub display: Display,
    /// The histogram strip under the image, besides the inspector's.
    pub histogram_overlay: bool,
    /// A crosshair at the cursor.
    pub crosshair: bool,
    #[facet(opaque)]
    db: Arc<DB>,
    #[facet(opaque)]
    view: ViewMode,
    /// Whether `display` is settled: set by the config, or by the
    /// producer's hints on the first frame.
    #[facet(opaque)]
    settled: bool,
    #[facet(opaque)]
    bound: Option<ComponentId>,
    #[facet(opaque)]
    frames_source: Option<Box<dyn FrameSource>>,
    #[facet(opaque)]
    state: FrameState,
    #[facet(opaque)]
    scheduler: Scheduler<Frame>,
    /// Why the wanted key's render failed.
    #[facet(opaque)]
    render_error: Option<RenderError>,
    #[facet(opaque)]
    frames: Frames,
    #[facet(opaque)]
    scratch: Option<Scratch>,
    #[facet(opaque)]
    lut: Option<(LutKey, Arc<Lut>)>,
    /// The levels the painted image used.
    #[facet(opaque)]
    levels: Option<(f64, f64)>,
    #[facet(opaque)]
    job: Task<()>,
    #[facet(opaque)]
    focus: FocusHandle,
    /// The free view a double click returns to from fit.
    #[facet(opaque)]
    last_free: Option<ViewMode>,
    /// Pan gesture: where it started and the viewport it started from.
    #[facet(opaque)]
    drag: Option<(Point<Pixels>, Viewport)>,
    /// The cursor, in window coordinates, while over the view.
    #[facet(opaque)]
    hover: Option<Point<Pixels>>,
    /// The canvas bounds, scale factor and device size of the last prepaint.
    #[facet(opaque)]
    bounds: Bounds<Pixels>,
    #[facet(opaque)]
    scale: f32,
    #[facet(opaque)]
    out: (usize, usize),
    /// Levels frozen by `lock`.
    #[facet(opaque)]
    locked_levels: Option<(f64, f64)>,
    /// The layout of the last frame, to notice a change.
    #[facet(opaque)]
    layout: Option<FrameLayout>,
    #[facet(opaque)]
    siblings: Siblings,
    #[facet(opaque)]
    overlays: Vec<Overlay>,
    #[facet(opaque)]
    _overlay_changes: Vec<Subscription>,
    /// The histogram of the painted image's region.
    #[facet(opaque)]
    histogram_data: Option<Arc<Histogram>>,
    /// Every live histogram strip; while any is, renders keep a histogram.
    #[facet(opaque)]
    strips: Vec<WeakEntity<HistogramStrip>>,
    /// The strip under the image, while the overlay is on.
    #[facet(opaque)]
    overlay_strip: Option<Entity<HistogramStrip>>,
    #[facet(opaque)]
    _observe: Option<Subscription>,
    #[facet(opaque)]
    _binding_changes: Task<()>,
}

impl ImageView {
    pub fn from_config(cfg: &ImageViewConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let source = Binding::from_text(&cfg.component, &db, cx);
        let view_mode = valid_view(cfg.view);
        let mut view = Self {
            source,
            display: cfg.display.unwrap_or_default(),
            histogram_overlay: cfg.histogram_overlay,
            crosshair: cfg.crosshair,
            view: view_mode,
            settled: cfg.display.is_some(),
            bound: None,
            frames_source: None,
            state: FrameState::Loading,
            scheduler: Scheduler::default(),
            render_error: None,
            frames: Frames::default(),
            scratch: Some(Scratch::default()),
            lut: None,
            levels: None,
            job: Task::ready(()),
            focus: cx.focus_handle(),
            last_free: matches!(view_mode, ViewMode::Free { .. }).then_some(view_mode),
            drag: None,
            hover: None,
            bounds: Bounds::default(),
            scale: 1.0,
            out: (0, 0),
            locked_levels: None,
            layout: None,
            siblings: Siblings::default(),
            overlays: Vec::new(),
            _overlay_changes: Vec::new(),
            histogram_data: None,
            strips: Vec::new(),
            overlay_strip: None,
            _observe: None,
            _binding_changes: crate::data_binding::watch_registrations(db.clone(), cx),
            db,
        };
        // A torn-out tab moves the view between windows, so release strips
        // every window's atlas, once in-flight frames are done with it.
        cx.on_release(|view, cx| crate::atlas::release_orphans(cx, view.frames.take_all()))
            .detach();
        for overlay in &cfg.overlays {
            view.add_overlay(overlay.clone(), cx);
        }
        view.rebind(cx);
        view
    }

    pub fn add_overlay(&mut self, config: OverlayConfig, cx: &mut Context<Self>) {
        let overlay = Overlay::new(&self.db, config, cx);
        self._overlay_changes.extend(overlay.observe(cx));
        self.overlays.push(overlay);
        cx.notify();
    }

    pub fn clear_overlays(&mut self, cx: &mut Context<Self>) {
        self.overlays.clear();
        self._overlay_changes.clear();
        cx.notify();
    }

    /// A view over a fixed source, for tests and previews.
    pub fn with_source(
        source: Box<dyn FrameSource>,
        display: Display,
        db: Arc<DB>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::from_config(
            &ImageViewConfig {
                display: Some(display),
                ..Default::default()
            },
            db,
            cx,
        );
        view._observe = source.observe(cx);
        view.frames_source = Some(source);
        view
    }

    /// Frozen levels are saved as manual ones, so a reload keeps them.
    pub fn to_config(&self) -> ImageViewConfig {
        let display = match (self.display.levels, self.locked_levels) {
            (Levels::Auto { lock: true, .. }, Some((black, white))) => Display {
                levels: Levels::Manual { black, white },
                ..self.display
            },
            _ => self.display,
        };
        ImageViewConfig {
            component: self.source.text(&self.db),
            display: self.settled.then_some(display),
            view: self.view,
            histogram_overlay: self.histogram_overlay,
            crosshair: self.crosshair,
            overlays: self.overlays.iter().map(|o| o.config.clone()).collect(),
        }
    }

    pub fn title(&self) -> SharedString {
        match self.source.text(&self.db) {
            name if name.is_empty() => "Image".into(),
            name => name.into(),
        }
    }

    /// The levels the painted image used.
    pub fn levels(&self) -> Option<(f64, f64)> {
        self.levels
    }

    pub fn state(&self) -> &FrameState {
        &self.state
    }

    /// The timestamp of the painted frame.
    #[cfg(test)]
    fn painted(&self) -> Option<metor_proto::types::Timestamp> {
        self.frames.current.as_ref()?;
        self.scheduler.installed().map(|key| key.timestamp)
    }

    /// Re-acquires the reader when the binding names another component.
    fn rebind(&mut self, cx: &mut Context<Self>) {
        self.source.resolve(&self.db, cx);
        let id = self.source.id();
        if self.bound == Some(id) {
            return;
        }
        self.bound = Some(id);
        self.forget_frame();
        if id == ComponentId(0) {
            self.frames_source = None;
            self._observe = None;
            return;
        }
        let source = RecordSource::new(self.db.clone(), id, cx);
        self._observe = source.observe(cx);
        self.frames_source = Some(Box::new(source));
    }

    pub(crate) fn source_changed(&mut self, cx: &mut Context<Self>) {
        cx.notify();
    }

    fn forget_frame(&mut self) {
        self.scheduler.reset();
        self.render_error = None;
        self.frames.install(None);
        self.forget_levels();
    }

    /// Levels belong to one layout; a new one starts over.
    fn forget_levels(&mut self) {
        self.levels = None;
        self.locked_levels = None;
        self.layout = None;
    }

    /// Refreshes the frame state and asks for a render at `out` device
    /// pixels.
    fn request(&mut self, out: (usize, usize), cx: &mut Context<Self>) {
        self.state = match &self.frames_source {
            Some(source) => source.frame(cx),
            None => FrameState::Loading,
        };
        let FrameState::Ready { frame, .. } = &self.state else {
            self.forget_frame();
            return;
        };
        let frame = frame.clone();
        if self.layout.is_some_and(|layout| layout != frame.layout) {
            self.forget_levels();
        }
        self.layout = Some(frame.layout);
        if !self.settled {
            self.display = render::hinted(self.display, &frame.layout);
            self.settled = true;
        }
        let viewport = self.viewport(&frame.layout);
        let display = self.effective_display();
        let mut key = RenderKey::new(&frame, display, viewport, out, saturated(cx));
        key.histogram = self.wants_histogram();
        if let Some(started) = self.scheduler.request(key, frame) {
            let job = self.job(started);
            self.job = spawn_jobs(job, cx);
        }
    }

    /// The display a render uses: `lock` swaps auto levels for the ones
    /// frozen when it was turned on.
    fn effective_display(&mut self) -> Display {
        let Levels::Auto { lock: true, .. } = self.display.levels else {
            self.locked_levels = None;
            return self.display;
        };
        if self.locked_levels.is_none() {
            self.locked_levels = self.levels;
        }
        match self.locked_levels {
            Some((black, white)) => Display {
                levels: Levels::Manual { black, white },
                ..self.display
            },
            None => self.display,
        }
    }

    /// The viewport over `layout` at the current output size.
    fn viewport(&self, layout: &FrameLayout) -> Viewport {
        match self.view {
            ViewMode::Fit => render::fit(layout, self.out),
            ViewMode::Free { center, step } => Viewport::centered(center, step, self.out),
        }
    }

    fn frame(&self) -> Option<&Frame> {
        match &self.state {
            FrameState::Ready { frame, .. } => Some(frame),
            _ => None,
        }
    }

    /// The current viewport, when a frame is showing.
    fn current_viewport(&self) -> Option<(Viewport, (usize, usize))> {
        let layout = &self.frame()?.layout;
        Some((self.viewport(layout), (layout.width, layout.height)))
    }

    /// A window position as a device position inside the view.
    fn device(&self, position: Point<Pixels>) -> (f64, f64) {
        let scale = f64::from(self.scale);
        let (x, _) = device_span(self.bounds.origin.x, self.bounds.size.width, self.scale);
        let (y, _) = device_span(self.bounds.origin.y, self.bounds.size.height, self.scale);
        (
            f64::from(position.x) * scale - x,
            f64::from(position.y) * scale - y,
        )
    }

    /// Switches to a free view at `viewport`, clamped so part of the image
    /// stays in view.
    fn set_free(&mut self, viewport: Viewport, img: (usize, usize), cx: &mut Context<Self>) {
        let viewport = viewport.clamped(img, self.out, PAN_MARGIN);
        if !viewport.is_valid() {
            return;
        }
        let free = ViewMode::Free {
            center: viewport.center(self.out),
            step: viewport.step,
        };
        self.view = free;
        self.last_free = Some(free);
        cx.notify();
    }

    /// Zooms by `factor` about a device point.
    pub fn zoom_at(&mut self, device: (f64, f64), factor: f64, cx: &mut Context<Self>) {
        if let Some((viewport, img)) = self.current_viewport() {
            self.set_free(viewport.zoom_about(device, factor), img, cx);
        }
    }

    /// Pans `start` by `delta` device pixels.
    pub fn pan(&mut self, start: Viewport, delta: (f64, f64), cx: &mut Context<Self>) {
        if let Some((_, img)) = self.current_viewport() {
            self.set_free(start.panned(delta), img, cx);
        }
    }

    pub fn fit(&mut self, cx: &mut Context<Self>) {
        self.view = ViewMode::Fit;
        cx.notify();
    }

    /// One source pixel per device pixel, about the current centre.
    pub fn one_to_one(&mut self, cx: &mut Context<Self>) {
        if let Some((viewport, img)) = self.current_viewport() {
            let center = viewport.center(self.out);
            self.set_free(Viewport::centered(center, 1.0, self.out), img, cx);
        }
    }

    /// Fit, or back to the last free view.
    pub fn toggle_fit(&mut self, cx: &mut Context<Self>) {
        match (self.view, self.last_free) {
            (ViewMode::Fit, Some(free)) => self.view = free,
            _ => self.view = ViewMode::Fit,
        }
        cx.notify();
    }

    /// The defaults and the producer's hints, and a fitted view.
    pub fn reset_to_metadata(&mut self, cx: &mut Context<Self>) {
        let hinted = self
            .frame()
            .map(|frame| render::hinted(Display::default(), &frame.layout));
        self.display = hinted.unwrap_or_default();
        self.locked_levels = None;
        self.view = ViewMode::Fit;
        cx.notify();
    }

    /// The pixel under the cursor and the scalar fields beside the image.
    /// The value is the source pixel painted at the cursor's device pixel.
    pub fn readout(&self, cx: &gpui::App) -> Option<Readout> {
        let frame = self.frame()?;
        let (dx, dy) = self.device(self.hover?);
        if dx < 0.0 || dy < 0.0 {
            return None;
        }
        let device = (dx.floor() as usize, dy.floor() as usize);
        let viewport = self.viewport(&frame.layout);
        let (x, y) = viewport.pixel_at(device);
        if x < 0.0 || y < 0.0 {
            return None;
        }
        let (x, y) = (x as usize, y as usize);
        Some(Readout {
            x,
            y,
            value: raw_value(frame, x, y)?,
            pooled: viewport.pooled(device),
            stars: self.stars_under(self.hover?, cx),
            siblings: self.siblings.values(&self.db, cx),
        })
    }

    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.modified() {
            return;
        }
        match event.keystroke.key.as_str() {
            "f" => self.fit(cx),
            "1" => self.one_to_one(cx),
            "r" => self.reset_to_metadata(cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus.focus(window, cx);
        cx.notify();
        if event.click_count == 2 {
            self.drag = None;
            self.toggle_fit(cx);
            return;
        }
        self.drag = self
            .current_viewport()
            .map(|(viewport, _)| (event.position, viewport));
    }

    fn on_hover_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        self.hover = Some(event.position);
        cx.notify();
    }

    fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// A move during a pan, wherever the cursor is.
    fn on_drag_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        if !event.dragging() {
            return;
        }
        if let Some((start, viewport)) = self.drag {
            let scale = f64::from(self.scale);
            let delta = (
                f64::from(event.position.x - start.x) * scale,
                f64::from(event.position.y - start.y) * scale,
            );
            self.pan(viewport, delta, cx);
        }
    }

    /// The release that ends any drag, wherever the cursor is.
    fn on_drag_end(&mut self, cx: &mut Context<Self>) {
        self.drag = None;
        cx.notify();
    }

    /// While a drag is on, follows the mouse across the whole window, so a
    /// release outside the view still ends it.
    fn listen_for_drag(&self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_dragging() {
            return;
        }
        let this = cx.entity().downgrade();
        let mover = this.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
            if phase == gpui::DispatchPhase::Bubble {
                let _ = mover.update(cx, |view, cx| view.on_drag_move(event, cx));
            }
        });
        window.on_mouse_event(move |event: &gpui::MouseUpEvent, phase, _, cx| {
            if phase == gpui::DispatchPhase::Bubble && event.button == MouseButton::Left {
                let _ = this.update(cx, |view, cx| view.on_drag_end(cx));
            }
        });
    }

    /// The window position of a source point.
    fn window_point(&self, viewport: &Viewport, source: (f64, f64)) -> Point<Pixels> {
        let device = viewport.to_device(source);
        let (x, _) = device_span(self.bounds.origin.x, self.bounds.size.width, self.scale);
        let (y, _) = device_span(self.bounds.origin.y, self.bounds.size.height, self.scale);
        let scale = f64::from(self.scale);
        point(
            px(((device.0 + x) / scale) as f32),
            px(((device.1 + y) / scale) as f32),
        )
    }

    fn overlay_color(color: OverlayColor, theme: &crate::theme::Theme) -> gpui::Hsla {
        match color {
            OverlayColor::Detected => theme.image_overlay_detected(),
            OverlayColor::Truth => theme.image_overlay_truth(),
        }
    }

    /// The overlay points under the cursor, described for the readout.
    fn stars_under(&self, cursor: Point<Pixels>, cx: &gpui::App) -> Vec<(OverlayColor, String)> {
        let Some((viewport, _)) = self.current_viewport() else {
            return Vec::new();
        };
        let to_window = |u, v| self.window_point(&viewport, (u, v));
        self.overlays
            .iter()
            .filter_map(|overlay| {
                let points = overlay.points(cx).ok()??;
                let kind = overlay.config.size_kind;
                let (cat_id, _) = overlay::under(&points, kind, cursor, to_window)?;
                let detail = match cat_id {
                    Some(id) => format!("cat {id}"),
                    None => "uncatalogued".to_string(),
                };
                Some((overlay.config.color, detail))
            })
            .collect()
    }

    /// Each overlay with points, and whether they are out of step: from
    /// another time than the frame, or with fields from different times.
    pub fn overlay_points(&self, cx: &gpui::App) -> Vec<(OverlayConfig, Points, bool)> {
        let frame_time = self.frame().map(|f| f.timestamp);
        self.overlays
            .iter()
            .filter_map(|overlay| {
                let points = overlay.points(cx).ok()??;
                let out_of_step = !points.consistent || Some(points.timestamp) != frame_time;
                Some((overlay.config.clone(), points, out_of_step))
            })
            .collect()
    }

    /// Overlay errors, one line each.
    fn overlay_errors(&self, cx: &gpui::App) -> Vec<String> {
        self.overlays
            .iter()
            .filter_map(|overlay| overlay.points(cx).err().map(|err| err.to_string()))
            .collect()
    }

    /// Paints the overlays and the crosshair.
    fn paint_annotations(&self, bounds: Bounds<Pixels>, window: &mut Window, cx: &gpui::App) {
        let Some(frame) = self.frame() else {
            return;
        };
        let theme = theme(cx);
        let viewport = self.viewport(&frame.layout);
        let to_window = |u: f64, v: f64| self.window_point(&viewport, (u, v));
        for (config, points, out_of_step) in self.overlay_points(cx) {
            let color = Self::overlay_color(config.color, &theme);
            overlay::paint(
                &points,
                config.size_kind,
                color,
                out_of_step,
                to_window,
                window,
            );
        }
        if let Some(hover) = self.hover.filter(|_| self.crosshair) {
            let color = theme.image_crosshair();
            let (w, h) = (bounds.size.width, bounds.size.height);
            window.paint_quad(gpui::fill(
                Bounds::new(point(bounds.origin.x, hover.y), size(w, px(1.0))),
                color,
            ));
            window.paint_quad(gpui::fill(
                Bounds::new(point(hover.x, bounds.origin.y), size(px(1.0), h)),
                color,
            ));
        }
    }

    /// A histogram strip for `view`, kept by the caller: the inspector's
    /// companion.
    pub fn histogram_strip(view: &Entity<Self>, cx: &mut gpui::App) -> Entity<HistogramStrip> {
        let strip = cx.new(|cx| HistogramStrip::new(view, cx));
        view.update(cx, |view, cx| view.add_strip(&strip, cx));
        strip
    }

    fn add_strip(&mut self, strip: &Entity<HistogramStrip>, cx: &mut Context<Self>) {
        self.strips.push(strip.downgrade());
        cx.notify();
    }

    /// Whether a live strip shows the histogram.
    fn wants_histogram(&mut self) -> bool {
        self.strips.retain(|strip| strip.upgrade().is_some());
        !self.strips.is_empty()
    }

    /// What a strip paints, once a render has made a histogram.
    pub(super) fn strip_data(&self) -> Option<StripData> {
        let top_code = self
            .frame()
            .filter(|f| f.layout.format.element() != metor_proto::types::PrimType::F32)
            .map(|f| f64::from((1u32 << f.layout.bits.min(16)) - 1));
        Some((self.histogram_data.clone()?, self.levels, top_code))
    }

    /// Sets how levels follow the frame, as the stretch presets do.
    pub fn set_stretch(&mut self, levels: Levels, cx: &mut Context<Self>) {
        self.display.levels = levels;
        self.locked_levels = None;
        cx.notify();
    }

    /// Opens this view's inspector page at `position`: the stretch, the
    /// histogram, and the view commands.
    fn open_menu(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.drag = None;
        window.dispatch_action(
            Box::new(crate::inspector::InspectEntity {
                entity: cx.entity().into_any(),
                position,
            }),
            cx,
        );
        cx.stop_propagation();
    }

    /// Manual levels, as the strip's handles set them.
    pub fn set_levels(&mut self, (black, white): (f64, f64), cx: &mut Context<Self>) {
        self.display.levels = Levels::Manual { black, white };
        self.locked_levels = None;
        self.levels = Some((black, white));
        cx.notify();
    }

    /// The strip under the image while the overlay is on, once there is a
    /// histogram to show.
    fn overlay(&mut self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        if !self.histogram_overlay {
            self.overlay_strip = None;
            return None;
        }
        if self.overlay_strip.is_none() {
            let view = cx.entity();
            let strip = cx.new(|cx| HistogramStrip::new(&view, cx));
            self.add_strip(&strip, cx);
            self.overlay_strip = Some(strip);
        }
        self.histogram_data.as_ref()?;
        Some(
            div()
                .h(px(strip::STRIP_HEIGHT))
                .w_full()
                .flex_shrink_0()
                .children(self.overlay_strip.clone()),
        )
    }

    /// Overlay errors, top left.
    fn info_chip(&self, cx: &gpui::App) -> Option<impl IntoElement> {
        let lines = self.overlay_errors(cx);
        if lines.is_empty() {
            return None;
        }
        Some(
            div().absolute().top(px(4.0)).left(px(4.0)).child(
                popover::readout_card(cx).children(
                    lines
                        .into_iter()
                        .map(|l| div().child(SharedString::from(l))),
                ),
            ),
        )
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(px(NOTCH_PX));
        let notches = -f64::from(f32::from(delta.y)) / f64::from(NOTCH_PX);
        self.zoom_at(
            self.device(event.position),
            ZOOM_PER_NOTCH.powf(notches),
            cx,
        );
        cx.stop_propagation();
    }

    fn job(&mut self, (key, frame): (RenderKey, Frame)) -> Job {
        Job {
            key,
            frame,
            scratch: self.scratch.take().unwrap_or_default(),
            lut: self.lut.take(),
        }
    }

    /// Installs a result if it is still wanted; returns the follow-up job.
    fn finished(&mut self, rendered: Rendered, cx: &mut Context<Self>) -> Option<Job> {
        self.scratch = Some(rendered.scratch);
        self.lut = rendered.lut;
        let (install, next) = self.scheduler.finish(&rendered.key);
        if install {
            self.levels = Some(rendered.levels);
            self.histogram_data = rendered.histogram;
            self.render_error = rendered.error;
            self.frames.install(rendered.image);
            cx.notify();
        }
        next.map(|started| self.job(started))
    }

    fn prepaint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.rebind(cx);
        if let Some(id) = self.bound.filter(|id| *id != ComponentId(0)) {
            self.siblings.refresh(&self.db, id, cx);
        }
        let scale = window.scale_factor();
        let (_, width) = device_span(bounds.origin.x, bounds.size.width, scale);
        let (_, height) = device_span(bounds.origin.y, bounds.size.height, scale);
        self.bounds = bounds;
        self.scale = scale;
        self.out = (width, height);
        self.request((width, height), cx);
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        for image in self.frames.take_retired() {
            crate::atlas::release_image(window, cx, image);
        }
        self.listen_for_drag(window, cx);
        window.paint_quad(gpui::fill(bounds, theme(cx).image_backdrop()));
        let Some((image, out)) = self.frames.current.clone() else {
            return;
        };
        let scale = window.scale_factor();
        let (x, _) = device_span(bounds.origin.x, bounds.size.width, scale);
        let (y, _) = device_span(bounds.origin.y, bounds.size.height, scale);
        // Until the render for a new pan or zoom lands, the last image moves
        // with the view, so it stays under the cursor and the overlays.
        let painted = self.scheduler.installed().map(|key| key.viewport);
        let current = self.frame().map(|frame| self.viewport(&frame.layout));
        let (dx, dy, dw, dh) = match (painted, current) {
            (Some(painted), Some(current)) => placed(out, &painted, &current),
            _ => (
                out.x as f64,
                out.y as f64,
                out.width as f64,
                out.height as f64,
            ),
        };
        let device = |v: f64| px((v / f64::from(scale)) as f32);
        let rect = Bounds::new(
            point(device(x + dx), device(y + dy)),
            size(device(dw), device(dh)),
        );
        let clip = bounds.intersect(&rect);
        let _ = window.paint_image(clip, rect, Corners::default(), image, 0, false);
        self.paint_annotations(bounds, window, cx);
    }

    fn status(&self) -> Option<(SharedString, bool)> {
        match &self.state {
            _ if self.source.id() == ComponentId(0) && self.frames_source.is_none() => Some((
                "no image bound — pick an image component in the inspector".into(),
                false,
            )),
            FrameState::Ready { .. } if self.render_error.is_some() => self
                .render_error
                .as_ref()
                .map(|err| (err.to_string().into(), false)),
            FrameState::Ready { stale: true, .. } => Some(("stale".into(), true)),
            FrameState::Ready { .. } => None,
            FrameState::Loading => Some(("loading…".into(), false)),
            FrameState::Missing => Some(("no frame at or before the view time".into(), false)),
            FrameState::Error(err) => Some((err.clone(), false)),
        }
    }
}

/// A configured view, or fit when a free view is not finite and positive.
fn valid_view(view: ViewMode) -> ViewMode {
    match view {
        ViewMode::Free { center, step }
            if center.0.is_finite() && center.1.is_finite() && step.is_finite() && step > 0.0 =>
        {
            ViewMode::Free {
                center,
                step: step.clamp(MIN_STEP, MAX_STEP),
            }
        }
        _ => ViewMode::Fit,
    }
}

/// Gap between the pointer and the hover readout, as the plots use.
const READOUT_OFFSET: f32 = 14.0;

/// The hover readout: the plots' readout card, placed beside the pointer
/// and flipped clear of the image's edges. The header names the pixel; the
/// rows are its value, the scalar siblings, and the overlay points under
/// the cursor with their overlay's swatch.
fn readout_card(
    readout: Readout,
    pointer: Point<Pixels>,
    bounds: Bounds<Pixels>,
    scale: f32,
    cx: &gpui::App,
) -> impl IntoElement + use<> {
    let theme = theme(cx);
    let header = match readout.pooled {
        Some((w, h)) => format!("x {}  y {}  (1 of {w}×{h})", readout.x, readout.y),
        None => format!("x {}  y {}", readout.x, readout.y),
    };
    let mut rows: Vec<(Option<gpui::Hsla>, SharedString, String, gpui::Hsla)> =
        vec![(None, "value".into(), readout.value, theme.text_primary)];
    rows.extend(
        readout
            .siblings
            .into_iter()
            .map(|(label, value)| (None, label, value, theme.text_primary)),
    );
    rows.extend(readout.stars.into_iter().map(|(role, detail)| {
        let color = ImageView::overlay_color(role, &theme);
        let label = match role {
            OverlayColor::Detected => "detected",
            OverlayColor::Truth => "truth",
        };
        (Some(color), label.into(), detail, color)
    }));

    let lens: Vec<(usize, usize)> = rows
        .iter()
        .map(|(_, label, value, _)| (label.chars().count(), value.chars().count()))
        .collect();
    let box_size = estimate_readout_size(header.chars().count(), &lens).map(|l| l * scale);
    let origin = readout_origin(pointer, box_size, bounds, px(READOUT_OFFSET * scale));

    let mut card = popover::readout_card(cx).child(div().child(SharedString::from(header)));
    for (swatch, label, value, value_color) in rows {
        let mut row = div().flex().flex_row().items_center().gap_1();
        if let Some(color) = swatch {
            row = row.child(div().w(ui_px(8.0)).h(ui_px(8.0)).rounded(px(2.0)).bg(color));
        }
        card = card.child(
            row.child(
                div()
                    .flex_1()
                    .pr(ui_px(8.0))
                    .text_color(theme.text_secondary)
                    .child(label),
            )
            .child(
                div()
                    .text_color(value_color)
                    .child(SharedString::from(value)),
            ),
        );
    }
    div()
        .absolute()
        .left(origin.x - bounds.origin.x)
        .top(origin.y - bounds.origin.y)
        .child(card)
}

/// The whole device pixels a logical span covers: its snapped start, and
/// its length.
fn device_span(origin: Pixels, len: Pixels, scale: f32) -> (f64, usize) {
    let scale = f64::from(scale);
    let start = (f64::from(origin) * scale).round();
    let end = ((f64::from(origin) + f64::from(len)) * scale).round();
    (start, (end - start).max(0.0) as usize)
}

/// Where an image rendered at `painted` covering device rect `out` lies
/// under `current`, as a device `(x, y, width, height)`. A pan at the same
/// step moves it by whole device pixels so it stays sharp.
fn placed(out: Rect, painted: &Viewport, current: &Viewport) -> (f64, f64, f64, f64) {
    let ratio = painted.step / current.step;
    let (x, y) = current.to_device(painted.to_source((out.x as f64, out.y as f64)));
    let (w, h) = (out.width as f64 * ratio, out.height as f64 * ratio);
    match painted.step == current.step {
        true => (x.round(), y.round(), w, h),
        false => (x, y, w, h),
    }
}

/// Runs `job` and its follow-ups one at a time on the background executor.
fn spawn_jobs(job: Job, cx: &mut Context<ImageView>) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let mut job = Some(job);
        while let Some(next) = job.take() {
            let rendered = cx
                .background_executor()
                .spawn(async move { render::run(next) })
                .await;
            job = this
                .update(cx, |view, cx| view.finished(rendered, cx))
                .ok()
                .flatten();
        }
    })
}

/// The theme's saturation colour as BGRA.
fn saturated(cx: &gpui::App) -> [u8; 4] {
    let c = gpui::Rgba::from(theme(cx).image_saturated());
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    [byte(c.b), byte(c.g), byte(c.r), byte(c.a)]
}

impl Render for ImageView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let this = cx.entity().downgrade();
        let painter = this.clone();
        let image = canvas(
            move |bounds, window, cx| {
                let _ = this.update(cx, |view, cx| view.prepaint(bounds, window, cx));
            },
            move |bounds, (), window, cx| {
                let _ = painter.update(cx, |view, cx| view.paint(bounds, window, cx));
            },
        )
        .size_full();
        let mut area = div()
            .id("image-view")
            .track_focus(&self.focus)
            .flex_1()
            .w_full()
            .relative()
            .overflow_hidden()
            .bg(theme.image_backdrop())
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, _, cx| view.on_key(event, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event: &MouseDownEvent, window, cx| {
                    view.on_mouse_down(event, window, cx)
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|view, event: &MouseDownEvent, window, cx| {
                    view.open_menu(event.position, window, cx)
                }),
            )
            .on_mouse_move(
                cx.listener(|view, event: &MouseMoveEvent, _, cx| view.on_hover_move(event, cx)),
            )
            .on_scroll_wheel(
                cx.listener(|view, event: &ScrollWheelEvent, _, cx| view.on_scroll(event, cx)),
            )
            .on_hover(cx.listener(|view, hovered: &bool, _, cx| {
                if !hovered {
                    view.hover = None;
                    cx.notify();
                }
            }))
            .child(image);
        if let (Some(readout), Some(pointer)) = (self.readout(cx), self.hover) {
            area = area.child(readout_card(
                readout,
                pointer,
                self.bounds,
                ui_scale(window),
                cx,
            ));
        }
        if let Some(info) = self.info_chip(cx) {
            area = area.child(info);
        }
        if let Some((text, badge)) = self.status() {
            let label = div()
                .px(px(4.0))
                .py(px(1.0))
                .text_size(px(11.0))
                .text_color(theme.text_secondary)
                .child(text);
            area = area.child(match badge {
                true => div()
                    .absolute()
                    .top(px(4.0))
                    .right(px(4.0))
                    .bg(theme.stale_bg())
                    .rounded(px(3.0))
                    .child(label.text_color(theme.stale_text())),
                false => div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(label),
            });
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(area)
            .children(self.overlay(cx))
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Entity, VisualTestContext};
    use metor_proto::types::Timestamp;

    use super::*;
    use crate::temporal::{TemporalController, TimeAction, TimeExpr, dispatch};
    use crate::views::image_view::source::StaticSource;
    use crate::views::image_view::source::tests::{ID, scrub_db};
    use crate::views::image_view::{FrameLayout, Levels};

    /// A window root that can let go of its image view.
    struct Host {
        view: Option<Entity<ImageView>>,
    }

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().children(self.view.clone())
        }
    }

    fn host(
        build: impl FnOnce(&mut Context<ImageView>) -> ImageView + 'static,
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<Host>, Entity<ImageView>, &mut VisualTestContext) {
        cx.update(|cx| {
            crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
            cx.set_global(crate::theme::FontSettings {
                family: "monospace".into(),
                config: Default::default(),
            });
        });
        let (host, vcx) = cx.add_window_view(|_, cx| Host {
            view: Some(cx.new(build)),
        });
        let view = vcx.update(|_, cx| host.read(cx).view.clone().unwrap());
        draw(vcx);
        (host, view, vcx)
    }

    /// Two passes: one requests a render, the next paints its result.
    fn draw(cx: &mut VisualTestContext) {
        for _ in 0..2 {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
        }
    }

    fn seek_to(t: i64, cx: &mut VisualTestContext) {
        cx.update(|_, cx| dispatch(TimeAction::Seek(TimeExpr::fixed(Timestamp(t))), cx).unwrap());
        draw(cx);
    }

    fn record_view(
        cx: &mut gpui::TestAppContext,
    ) -> (
        tempfile::TempDir,
        Entity<Host>,
        Entity<ImageView>,
        &mut VisualTestContext,
    ) {
        let (temp, db, _component) = scrub_db(&[10, 20, 100]);
        cx.update(|cx| {
            TemporalController::init(db.clone(), cx);
        });
        let cfg = ImageViewConfig {
            component: ID.into(),
            ..Default::default()
        };
        let (host, view, vcx) = host(move |cx| ImageView::from_config(&cfg, db, cx), cx);
        (temp, host, view, vcx)
    }

    #[gpui::test]
    fn the_rendered_frame_follows_the_seek(cx: &mut gpui::TestAppContext) {
        let (_temp, _host, view, vcx) = record_view(cx);
        for (t, want) in [
            (15, Some(10)),
            (20, Some(20)),
            (99, Some(20)),
            (5, None),
            (100, Some(100)),
        ] {
            seek_to(t, vcx);
            let painted = vcx.update(|_, cx| view.read(cx).painted());
            assert_eq!(painted, want.map(Timestamp), "seek {t}");
        }
        let state = vcx.update(|_, cx| view.read(cx).state().clone());
        assert!(matches!(state, FrameState::Ready { .. }));
    }

    /// Runs enough frames, and waits long enough, for every parked image to
    /// leave the atlas.
    fn settle(vcx: &mut VisualTestContext) {
        std::thread::sleep(crate::atlas::SETTLE_TIME);
        for _ in 0..crate::atlas::SETTLE_FRAMES {
            vcx.update(|window, cx| window.simulate_next_frame(cx));
        }
    }

    #[gpui::test]
    fn only_the_current_image_stays_in_the_atlas(cx: &mut gpui::TestAppContext) {
        let (_temp, host, view, vcx) = record_view(cx);
        let mut images = Vec::new();
        for t in [10, 20, 100] {
            seek_to(t, vcx);
            let current = vcx.update(|_, cx| view.read(cx).frames.current.clone());
            images.push(current.expect("an image").0);
        }
        let held = |vcx: &mut VisualTestContext| {
            vcx.update(|window, _| {
                images
                    .iter()
                    .map(|i| window.has_image_atlas_entry(i))
                    .collect::<Vec<_>>()
            })
        };
        // Replaced images stay until in-flight frames are done with them,
        // so their atlas slots cannot be refilled under a frame on the GPU.
        assert_eq!(held(vcx), [true, true, true]);
        settle(vcx);
        assert_eq!(held(vcx), [false, false, true]);
        drop(view);
        vcx.update(|_, cx| {
            host.update(cx, |host, cx| {
                host.view = None;
                cx.notify();
            })
        });
        draw(vcx);
        assert_eq!(held(vcx), [false, false, true]);
        vcx.executor().advance_clock(crate::atlas::ORPHAN_WAIT);
        vcx.run_until_parked();
        assert_eq!(held(vcx), [false, false, false]);
    }

    #[gpui::test]
    fn a_static_frame_renders_with_auto_levels(cx: &mut gpui::TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
        let bytes: Vec<u8> = (0..16u16).flat_map(|v| (v * 100).to_le_bytes()).collect();
        let layout = FrameLayout::from_component(
            &metor_db::ComponentSchema::new(metor_proto::types::PrimType::U16, &[4usize, 4][..]),
            &metor_proto_wkt::ComponentMetadata::from("cam")
                .with_image(metor_proto_wkt::ImageFormat::Mono16, 12),
        )
        .unwrap();
        let frame = Frame {
            timestamp: Timestamp(1),
            bytes: bytes.into(),
            layout,
        };
        let source = StaticSource(FrameState::Ready {
            frame,
            stale: false,
        });
        let (_host, view, vcx) = host(
            move |cx| ImageView::with_source(Box::new(source), Display::default(), db, cx),
            cx,
        );
        vcx.update(|_, cx| {
            let view = view.read(cx);
            assert_eq!(view.levels(), Some((0.0, 1500.0)));
            assert_eq!(view.painted(), Some(Timestamp(1)));
            assert!(matches!(view.display.levels, Levels::Auto { .. }));
        });
    }

    #[gpui::test]
    fn an_error_frame_shows_its_text(cx: &mut gpui::TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
        let source = StaticSource(FrameState::Error("unknown image.format".into()));
        let (_host, view, vcx) = host(
            move |cx| ImageView::with_source(Box::new(source), Display::default(), db, cx),
            cx,
        );
        vcx.update(|_, cx| {
            let view = view.read(cx);
            assert_eq!(view.status(), Some(("unknown image.format".into(), false)));
            assert!(view.frames.current.is_none());
        });
    }

    /// A torn-out tab: painted in one window, moved to another, then
    /// released. Neither window keeps the image.
    #[gpui::test]
    fn release_strips_every_window(cx: &mut gpui::TestAppContext) {
        let (_temp, db, _component) = scrub_db(&[10]);
        cx.update(|cx| {
            crate::theme::set_theme(cx, Arc::new(crate::theme::DARK.clone()));
            TemporalController::init(db.clone(), cx);
            dispatch(TimeAction::Seek(TimeExpr::fixed(Timestamp(10))), cx).unwrap();
        });
        let cfg = ImageViewConfig {
            component: ID.into(),
            ..Default::default()
        };
        let a = cx.add_window(move |_, cx| Host {
            view: Some(cx.new(|cx| ImageView::from_config(&cfg, db, cx))),
        });
        let refresh = |cx: &mut gpui::TestAppContext, w: gpui::WindowHandle<Host>| {
            for _ in 0..2 {
                cx.update_window(w.into(), |_, window, _| window.refresh())
                    .unwrap();
                cx.run_until_parked();
            }
        };
        refresh(cx, a);
        let view = cx.update(|cx| a.read(cx).unwrap().view.clone().unwrap());
        let image = cx
            .update(|cx| view.read(cx).frames.current.clone())
            .expect("an image")
            .0;
        let moved = view.clone();
        let b = cx.add_window(move |_, _| Host { view: Some(moved) });
        cx.update(|cx| {
            a.update(cx, |host, _, cx| {
                host.view = None;
                cx.notify();
            })
            .unwrap()
        });
        refresh(cx, a);
        refresh(cx, b);
        let held = |cx: &mut gpui::TestAppContext, w: gpui::WindowHandle<Host>| {
            cx.update_window(w.into(), |_, window, _| {
                window.has_image_atlas_entry(&image)
            })
            .unwrap()
        };
        assert!(held(cx, a) && held(cx, b));
        drop(view);
        cx.update(|cx| {
            b.update(cx, |host, _, cx| {
                host.view = None;
                cx.notify();
            })
            .unwrap()
        });
        refresh(cx, b);
        assert!(held(cx, a) && held(cx, b), "dropped under in-flight frames");
        cx.executor().advance_clock(crate::atlas::ORPHAN_WAIT);
        cx.run_until_parked();
        assert!(!held(cx, a) && !held(cx, b));
    }

    /// A source whose state the test sets.
    struct Switch(std::rc::Rc<std::cell::RefCell<FrameState>>);

    impl FrameSource for Switch {
        fn frame(&self, _: &gpui::App) -> FrameState {
            self.0.borrow().clone()
        }

        fn observe(&self, _: &mut Context<ImageView>) -> Option<Subscription> {
            None
        }
    }

    #[gpui::test]
    fn a_disappearing_component_shows_loading(cx: &mut gpui::TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
        let layout = FrameLayout::from_component(
            &metor_db::ComponentSchema::new(metor_proto::types::PrimType::U16, &[2usize, 2][..]),
            &metor_proto_wkt::ComponentMetadata::from("cam")
                .with_image(metor_proto_wkt::ImageFormat::Mono16, 12),
        )
        .unwrap();
        let frame = Frame {
            timestamp: Timestamp(1),
            bytes: vec![0u8; 8].into(),
            layout,
        };
        let state = std::rc::Rc::new(std::cell::RefCell::new(FrameState::Ready {
            frame,
            stale: false,
        }));
        let source = Switch(state.clone());
        let (_host, view, vcx) = host(
            move |cx| ImageView::with_source(Box::new(source), Display::default(), db, cx),
            cx,
        );
        assert!(vcx.update(|_, cx| view.read(cx).frames.current.is_some()));
        *state.borrow_mut() = FrameState::Loading;
        vcx.update(|_, cx| view.update(cx, |_, cx| cx.notify()));
        draw(vcx);
        vcx.update(|_, cx| {
            let view = view.read(cx);
            assert_eq!(view.status(), Some(("loading…".into(), false)));
            assert!(view.frames.current.is_none());
        });
    }

    #[gpui::test]
    fn an_rgb_frame_renders_and_a_render_error_shows(cx: &mut gpui::TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
        let layout = FrameLayout::from_component(
            &metor_db::ComponentSchema::new(metor_proto::types::PrimType::U8, &[2usize, 2, 3][..]),
            &metor_proto_wkt::ComponentMetadata::from("cam")
                .with_image(metor_proto_wkt::ImageFormat::Rgb8, 8),
        )
        .unwrap();
        let frame = Frame {
            timestamp: Timestamp(1),
            bytes: vec![0u8; 12].into(),
            layout,
        };
        let source = StaticSource(FrameState::Ready {
            frame,
            stale: false,
        });
        let (_host, view, vcx) = host(
            move |cx| ImageView::with_source(Box::new(source), Display::default(), db, cx),
            cx,
        );
        vcx.update(|_, cx| {
            let view = view.read(cx);
            assert_eq!(view.status(), None);
            assert!(view.frames.current.is_some());
        });
        vcx.update(|_, cx| {
            view.update(cx, |view, _| {
                view.render_error = Some(RenderError::InvalidViewport)
            });
            let view = view.read(cx);
            assert_eq!(
                view.status(),
                Some(("the viewport is not finite and positive".into(), false))
            );
        });
    }

    /// An 8×8 Mono16 frame whose pixel `(x, y)` is `x + 8y`.
    fn ramp_view(
        cx: &mut gpui::TestAppContext,
    ) -> (tempfile::TempDir, Entity<ImageView>, &mut VisualTestContext) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
        let bytes: Vec<u8> = (0..64u16).flat_map(|v| v.to_le_bytes()).collect();
        let layout = FrameLayout::from_component(
            &metor_db::ComponentSchema::new(metor_proto::types::PrimType::U16, &[8usize, 8][..]),
            &metor_proto_wkt::ComponentMetadata::from("cam")
                .with_image(metor_proto_wkt::ImageFormat::Mono16, 12),
        )
        .unwrap();
        let frame = Frame {
            timestamp: Timestamp(1),
            bytes: bytes.into(),
            layout,
        };
        let source = StaticSource(FrameState::Ready {
            frame,
            stale: false,
        });
        let (_host, view, vcx) = host(
            move |cx| ImageView::with_source(Box::new(source), Display::default(), db, cx),
            cx,
        );
        (temp, view, vcx)
    }

    /// The window position over the centre of source pixel `(x, y)`.
    fn over(view: &ImageView, x: f64, y: f64) -> Point<Pixels> {
        let frame = view.frame().unwrap();
        let device = view.viewport(&frame.layout).to_device((x + 0.5, y + 0.5));
        let (start_x, _) = device_span(view.bounds.origin.x, view.bounds.size.width, view.scale);
        let (start_y, _) = device_span(view.bounds.origin.y, view.bounds.size.height, view.scale);
        let scale = f64::from(view.scale);
        point(
            px(((device.0 + start_x) / scale) as f32),
            px(((device.1 + start_y) / scale) as f32),
        )
    }

    #[gpui::test]
    fn the_readout_follows_zoom_and_pan(cx: &mut gpui::TestAppContext) {
        let (_temp, view, vcx) = ramp_view(cx);
        vcx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let out = view.out;
                assert!(out.0 > 0 && out.1 > 0);
                view.zoom_at((out.0 as f64 / 2.0, out.1 as f64 / 2.0), 2.0, cx);
                let (start, _) = view.current_viewport().unwrap();
                view.pan(start, (-30.0, 12.0), cx);
                assert!(matches!(view.view, ViewMode::Free { .. }));
            })
        });
        draw(vcx);
        for (x, y) in [(3, 5), (0, 0), (7, 7)] {
            let readout = vcx.update(|_, cx| {
                view.update(cx, |view, _| {
                    view.hover = Some(over(view, x as f64, y as f64))
                });
                view.read(cx).readout(cx)
            });
            let readout = readout.expect("a readout");
            assert_eq!((readout.x, readout.y), (x, y));
            assert_eq!(readout.value, (x + 8 * y).to_string());
        }
        vcx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.hover = Some(point(px(-1000.0), px(-1000.0)));
                assert_eq!(view.readout(cx), None);
                view.toggle_fit(cx);
                assert_eq!(view.view, ViewMode::Fit);
                view.toggle_fit(cx);
                assert!(matches!(view.view, ViewMode::Free { .. }));
                view.one_to_one(cx);
                assert!(matches!(view.view, ViewMode::Free { step, .. } if step == 1.0));
                view.reset_to_metadata(cx);
                assert_eq!(view.view, ViewMode::Fit);
            })
        });
    }

    /// Levels after showing frame 10, setting `lock`, then seeking to 20.
    /// The window position of a device position inside the view.
    fn at_device(view: &ImageView, device: (f64, f64)) -> Point<Pixels> {
        let (start_x, _) = device_span(view.bounds.origin.x, view.bounds.size.width, view.scale);
        let (start_y, _) = device_span(view.bounds.origin.y, view.bounds.size.height, view.scale);
        let scale = f64::from(view.scale);
        point(
            px(((device.0 + start_x) / scale) as f32),
            px(((device.1 + start_y) / scale) as f32),
        )
    }

    /// The ramp value painted at the device pixel under `position`.
    fn painted_value(view: &ImageView, position: Point<Pixels>) -> u16 {
        let (image, out) = view.frames.current.clone().expect("an image");
        let (dx, dy) = view.device(position);
        let (x, y) = (dx.floor() as usize - out.x, dy.floor() as usize - out.y);
        let gray = image.as_bytes(0).unwrap()[(y * out.width + x) * 4 + 2];
        (0..64u16)
            .find(|v| (f64::from(*v) * 255.0 / 63.0).round() as u8 == gray)
            .expect("a ramp gray")
    }

    #[gpui::test]
    fn the_readout_matches_the_painted_pixel(cx: &mut gpui::TestAppContext) {
        let (_temp, view, vcx) = ramp_view(cx);
        vcx.update(|_, cx| {
            view.update(cx, |view, cx| {
                // Close to 1:1, so source and device pixels are similar in
                // size and their edges interleave.
                let start = Viewport::centered((3.37, 3.61), 0.9, view.out);
                view.set_free(start, (8, 8), cx);
                let out = view.out;
                view.zoom_at(
                    (out.0 as f64 / 3.0 + 0.3, out.1 as f64 / 3.0 + 0.7),
                    1.07,
                    cx,
                );
                let ViewMode::Free { step, .. } = view.view else {
                    panic!("a free view");
                };
                let (viewport, _) = view.current_viewport().unwrap();
                assert!(step < 1.0 && viewport.origin.0.fract() != 0.0);
            })
        });
        draw(vcx);
        // Device pixels whose left edge sits on source pixel `x` but whose
        // sample, at the pixel centre, is `x + 1`: a cursor there reads
        // `x` by plain flooring, but the painted pixel is `x + 1`.
        let straddling = vcx.update(|_, cx| {
            let view = view.read(cx);
            let viewport = view.viewport(&view.frame().unwrap().layout);
            (0..view.out.0)
                .filter(|&i| {
                    let edge = viewport.to_source((i as f64 + 0.01, 0.0)).0.floor();
                    (0.0..7.0).contains(&edge) && viewport.pixel_at((i, 0)).0 != edge
                })
                .take(3)
                .collect::<Vec<_>>()
        });
        assert!(!straddling.is_empty(), "the zoom left no straddling pixel");
        for i in straddling {
            let (readout, painted) = vcx.update(|_, cx| {
                view.update(cx, |view, _| {
                    let viewport = view.viewport(&view.frame().unwrap().layout);
                    let y = viewport.to_device((0.0, 3.5)).1;
                    view.hover = Some(at_device(view, (i as f64 + 0.01, y)));
                });
                let view = view.read(cx);
                (
                    view.readout(cx).expect("a readout"),
                    painted_value(view, view.hover.unwrap()),
                )
            });
            assert_eq!(readout.value, painted.to_string(), "device {i}");
            assert_eq!(readout.pooled, None);
        }
    }

    #[gpui::test]
    fn a_zoomed_out_readout_names_its_block(cx: &mut gpui::TestAppContext) {
        let (_temp, view, vcx) = ramp_view(cx);
        let readout = vcx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.set_free(Viewport::centered((4.0, 4.0), 4.0, view.out), (8, 8), cx);
                let device = view
                    .viewport(&view.frame().unwrap().layout)
                    .to_device((5.0, 5.0));
                view.hover = Some(at_device(view, device));
                view.readout(cx)
            })
        });
        assert_eq!(readout.expect("a readout").pooled, Some((4, 4)));
    }

    #[gpui::test]
    fn keys_without_modifiers_navigate(cx: &mut gpui::TestAppContext) {
        let (_temp, view, vcx) = ramp_view(cx);
        vcx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.focus.focus(window, cx);
                view.one_to_one(cx);
            })
        });
        vcx.simulate_keystrokes("cmd-f");
        assert!(vcx.update(|_, cx| matches!(view.read(cx).view, ViewMode::Free { .. })));
        vcx.simulate_keystrokes("f");
        assert_eq!(vcx.update(|_, cx| view.read(cx).view), ViewMode::Fit);
        vcx.simulate_keystrokes("1");
        assert!(vcx.update(
            |_, cx| matches!(view.read(cx).view, ViewMode::Free { step, .. } if step == 1.0)
        ));
    }

    #[test]
    fn a_bad_free_view_from_config_is_fit() {
        for (center, step) in [
            ((f64::NAN, 0.0), 1.0),
            ((0.0, f64::INFINITY), 1.0),
            ((0.0, 0.0), 0.0),
            ((0.0, 0.0), -2.0),
            ((0.0, 0.0), f64::NAN),
        ] {
            assert_eq!(valid_view(ViewMode::Free { center, step }), ViewMode::Fit);
        }
        assert_eq!(
            valid_view(ViewMode::Free {
                center: (1.0, 2.0),
                step: 1e9
            }),
            ViewMode::Free {
                center: (1.0, 2.0),
                step: MAX_STEP
            }
        );
    }

    #[gpui::test]
    fn a_layout_change_drops_the_lock(cx: &mut gpui::TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
        let frame = |format, prim, value: u8| {
            let layout = FrameLayout::from_component(
                &metor_db::ComponentSchema::new(prim, &[2usize, 2][..]),
                &metor_proto_wkt::ComponentMetadata::from("cam").with_image(format, 8),
            )
            .unwrap();
            let bytes: Vec<u8> = match prim {
                metor_proto::types::PrimType::U8 => vec![value; 4],
                _ => (0..4)
                    .flat_map(|_| u16::from(value).to_le_bytes())
                    .collect(),
            };
            FrameState::Ready {
                frame: Frame {
                    timestamp: Timestamp(1),
                    bytes: bytes.into(),
                    layout,
                },
                stale: false,
            }
        };
        use metor_proto::types::PrimType;
        use metor_proto_wkt::ImageFormat;
        let state = std::rc::Rc::new(std::cell::RefCell::new(frame(
            ImageFormat::Mono16,
            PrimType::U16,
            200,
        )));
        let source = Switch(state.clone());
        let locked = Display {
            levels: Levels::Auto {
                low: 0.0,
                high: 1.0,
                lock: true,
            },
            ..Display::default()
        };
        let (_host, view, vcx) = host(
            move |cx| ImageView::with_source(Box::new(source), locked, db, cx),
            cx,
        );
        draw(vcx);
        let cfg = vcx.update(|_, cx| view.read(cx).to_config());
        assert_eq!(
            cfg.display.unwrap().levels,
            Levels::Manual {
                black: 200.0,
                white: 200.0
            }
        );
        *state.borrow_mut() = frame(ImageFormat::Mono8, PrimType::U8, 7);
        vcx.update(|_, cx| view.update(cx, |_, cx| cx.notify()));
        draw(vcx);
        draw(vcx);
        assert_eq!(vcx.update(|_, cx| view.read(cx).levels()), Some((7.0, 7.0)));
    }

    #[gpui::test]
    fn the_inspector_source_lists_only_images(cx: &mut gpui::TestAppContext) {
        let (db, _temp) = crate::views::image_view::tests::image_db();
        let inspect_db = db.clone();
        cx.update(|cx| {
            crate::inspector::registry::InspectorRegistry::init(db.clone(), cx);
        });
        let cfg = ImageViewConfig {
            component: "cam.pixels".into(),
            ..Default::default()
        };
        let (_host, view, vcx) = host(move |cx| ImageView::from_config(&cfg, db, cx), cx);
        let labels = vcx.update(|window, cx| {
            let mut rows = crate::inspector::reflect::rows_for_entity(&view, &inspect_db, cx);
            let source = rows
                .iter_mut()
                .find(|r| r.label() == "Source")
                .expect("a source row");
            let crate::inspector::rows::RowAction::Cascade(page) = source.activate(window, cx)
            else {
                panic!("the source row opens a page");
            };
            page.iter()
                .map(|r| r.label().to_string())
                .collect::<Vec<_>>()
        });
        assert!(labels.contains(&"cam.pixels".to_string()), "{labels:?}");
        assert!(labels.contains(&"Clear".to_string()), "{labels:?}");
        assert!(
            !labels.iter().any(|l| l == "cam.exposure" || l == "cam.raw"),
            "{labels:?}"
        );
    }

    fn levels_after_seeks(lock: bool, cx: &mut gpui::TestAppContext) -> Vec<Option<(f64, f64)>> {
        let (_temp, db, _component) = scrub_db(&[10, 20]);
        cx.update(|cx| {
            TemporalController::init(db.clone(), cx);
        });
        let auto = |lock| Display {
            levels: Levels::Auto {
                low: 0.0,
                high: 1.0,
                lock,
            },
            ..Display::default()
        };
        let cfg = ImageViewConfig {
            component: ID.into(),
            display: Some(auto(false)),
            ..Default::default()
        };
        let (_host, view, vcx) = host(move |cx| ImageView::from_config(&cfg, db, cx), cx);
        seek_to(10, vcx);
        let first = vcx.update(|_, cx| view.read(cx).levels());
        vcx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.display = auto(lock);
                cx.notify();
            })
        });
        draw(vcx);
        seek_to(20, vcx);
        vec![first, vcx.update(|_, cx| view.read(cx).levels())]
    }

    #[gpui::test]
    fn lock_holds_the_levels_across_frames(cx: &mut gpui::TestAppContext) {
        let locked = levels_after_seeks(true, cx);
        assert_eq!(locked, [Some((100.0, 100.0)), Some((100.0, 100.0))]);
    }

    #[gpui::test]
    fn unlocked_levels_follow_each_frame(cx: &mut gpui::TestAppContext) {
        let free = levels_after_seeks(false, cx);
        assert_eq!(free, [Some((100.0, 100.0)), Some((200.0, 200.0))]);
    }

    #[gpui::test]
    fn the_readout_shows_scalar_siblings(cx: &mut gpui::TestAppContext) {
        let (_temp, db, _component) = scrub_db(&[10]);
        let exposure = ComponentId::new("cam.exposure");
        db.with_state_mut(|s| {
            s.set_component_metadata(
                metor_proto_wkt::ComponentMetadata::from("cam.exposure"),
                &db.path,
            )?;
            s.insert_component(
                exposure,
                metor_db::ComponentSchema::new(metor_proto::types::PrimType::F64, &[][..]),
                &db.path,
            )
        })
        .unwrap();
        let component = db
            .with_state(|s| s.get_component(exposure).cloned())
            .unwrap();
        component
            .time_series
            .install_samples(
                8,
                [(Timestamp(5), &0.25f64.to_le_bytes()[..])],
                metor_db::manifest::SpanSource::RemoteFetch,
            )
            .unwrap();
        cx.update(|cx| {
            TemporalController::init(db.clone(), cx);
        });
        let cfg = ImageViewConfig {
            component: ID.into(),
            ..Default::default()
        };
        let (_host, view, vcx) = host(move |cx| ImageView::from_config(&cfg, db, cx), cx);
        seek_to(10, vcx);
        let readout = vcx.update(|_, cx| {
            view.update(cx, |view, _| view.hover = Some(over(view, 1.0, 1.0)));
            view.read(cx).readout(cx)
        });
        let readout = readout.expect("a readout");
        assert_eq!(readout.value, "100");
        assert_eq!(readout.siblings.len(), 1);
        assert_eq!(readout.siblings[0].0.as_ref(), "exposure");
        assert!(
            readout.siblings[0].1.contains("0.25"),
            "{:?}",
            readout.siblings
        );
    }

    #[gpui::test]
    fn the_inspector_offers_display_and_view_commands(cx: &mut gpui::TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
        let inspect_db = db.clone();
        cx.update(|cx| {
            crate::inspector::registry::InspectorRegistry::init(db.clone(), cx);
        });
        let source = StaticSource(FrameState::Loading);
        let (_host, view, vcx) = host(
            move |cx| ImageView::with_source(Box::new(source), Display::default(), db, cx),
            cx,
        );
        let (labels, expanded) = vcx.update(|_, cx| {
            let rows = crate::inspector::reflect::rows_for_entity(&view, &inspect_db, cx);
            // The levels row carries the histogram beneath the page.
            let expanded = rows
                .iter()
                .find(|row| row.label() == "Levels")
                .and_then(|row| row.accessory("", cx))
                .map(|spec| spec.expanded);
            (
                rows.iter()
                    .map(|row| row.label().to_string())
                    .collect::<Vec<_>>(),
                expanded,
            )
        });
        assert_eq!(expanded, Some(true));
        for want in [
            "Fit (F)",
            "1:1 (1)",
            "Reset to metadata (R)",
            "Levels",
            "Stretch: min–max",
            "Stretch: equalize",
        ] {
            assert!(labels.iter().any(|l| l == want), "{want} in {labels:?}");
        }
        assert!(labels.len() > 3, "{labels:?}");
    }

    #[test]
    fn device_spans_snap_both_edges() {
        assert_eq!(device_span(px(10.3), px(20.0), 2.0), (21.0, 40));
        assert_eq!(device_span(px(0.25), px(0.5), 1.0), (0.0, 1));
        assert_eq!(device_span(px(5.0), px(0.0), 2.0), (10.0, 0));
    }

    #[test]
    fn the_last_image_follows_a_pan_and_a_zoom() {
        let painted = Viewport {
            origin: (10.0, 20.0),
            step: 0.5,
        };
        let out = Rect::new(4, 6, 100, 50);
        assert_eq!(placed(out, &painted, &painted), (4.0, 6.0, 100.0, 50.0));
        // Dragging right and down by (30.4, -12.6) device pixels.
        let panned = painted.panned((30.4, -12.6));
        assert_eq!(placed(out, &painted, &panned), (34.0, -7.0, 100.0, 50.0));
        // Zooming in 2× about device (54, 31), the middle of the image.
        let zoomed = painted.zoom_about((54.0, 31.0), 2.0);
        let (x, y, w, h) = placed(out, &painted, &zoomed);
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(close(x, -46.0) && close(y, -19.0), "{x} {y}");
        assert!(close(w, 200.0) && close(h, 100.0), "{w} {h}");
    }

    /// Between a pan and its render, the painted image moves with the view.
    #[gpui::test]
    fn a_pan_moves_the_painted_image_before_its_render(cx: &mut gpui::TestAppContext) {
        let (_temp, view, vcx) = ramp_view(cx);
        vcx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let out = view.out;
                view.zoom_at((out.0 as f64 / 2.0, out.1 as f64 / 2.0), 2.0, cx);
            });
        });
        draw(vcx);
        let (installed, panned, out) = vcx.update(|_, cx| {
            view.update(cx, |view, cx| {
                let (start, _) = view.current_viewport().unwrap();
                view.pan(start, (17.0, -9.0), cx);
                let (panned, _) = view.current_viewport().unwrap();
                let installed = view.scheduler.installed().unwrap().viewport;
                (installed, panned, view.frames.current.clone().unwrap().1)
            })
        });
        assert_ne!(installed, panned, "the render has not landed yet");
        let (x, y, _, _) = placed(out, &installed, &panned);
        assert_eq!((x, y), (out.x as f64 + 17.0, out.y as f64 - 9.0));
    }

    /// A `[32]` centroid record at `cam.truth`: one row per sample, at
    /// `(t, u)` with `v = u`.
    fn install_truth(db: &DB, samples: &[(i64, f32)]) {
        use metor_db::ComponentSchema;
        use metor_db::manifest::SpanSource;
        use metor_proto::types::PrimType;
        for (field, schema) in [
            ("u", ComponentSchema::new(PrimType::F32, &[32usize][..])),
            ("v", ComponentSchema::new(PrimType::F32, &[32usize][..])),
            ("count", ComponentSchema::new(PrimType::U64, &[][..])),
        ] {
            let name = format!("cam.truth.{field}");
            let id = ComponentId::new(&name);
            db.with_state_mut(|s| {
                s.set_component_metadata(
                    metor_proto_wkt::ComponentMetadata::from(name.as_str()),
                    &db.path,
                )?;
                s.insert_component(id, schema.clone(), &db.path)
            })
            .unwrap();
            let bytes: Vec<Vec<u8>> = samples
                .iter()
                .map(|&(_, u)| match field {
                    "count" => 1u64.to_le_bytes().to_vec(),
                    _ => (0..32)
                        .flat_map(|i| if i == 0 { u } else { 0.0 }.to_le_bytes())
                        .collect(),
                })
                .collect();
            let component = db.with_state(|s| s.get_component(id).cloned()).unwrap();
            component
                .time_series
                .install_samples(
                    schema.size(),
                    samples
                        .iter()
                        .zip(&bytes)
                        .map(|((t, _), b)| (Timestamp(*t), b.as_slice())),
                    SpanSource::RemoteFetch,
                )
                .unwrap();
        }
    }

    fn truth_view<'a>(
        samples: &[(i64, f32)],
        cx: &'a mut gpui::TestAppContext,
    ) -> (
        tempfile::TempDir,
        Entity<ImageView>,
        &'a mut VisualTestContext,
    ) {
        let (temp, db, _component) = scrub_db(&[10, 20]);
        install_truth(&db, samples);
        cx.update(|cx| {
            TemporalController::init(db.clone(), cx);
        });
        let cfg = ImageViewConfig {
            component: ID.into(),
            overlays: vec![OverlayConfig {
                size: None,
                cat_id: None,
                ..OverlayConfig::from_root("cam.truth", OverlayColor::Truth)
            }],
            ..Default::default()
        };
        let (_host, view, vcx) = host(move |cx| ImageView::from_config(&cfg, db, cx), cx);
        (temp, view, vcx)
    }

    fn first_point(view: &Entity<ImageView>, cx: &mut VisualTestContext) -> Option<(f32, bool)> {
        cx.update(|_, cx| {
            let points = view.read(cx).overlay_points(cx);
            points.first().map(|(_, p, stale)| (p.stars[0].u, *stale))
        })
    }

    #[gpui::test]
    fn the_overlay_follows_a_seek(cx: &mut gpui::TestAppContext) {
        let (_temp, view, vcx) = truth_view(&[(10, 1.5), (20, 2.5)], cx);
        seek_to(10, vcx);
        assert_eq!(first_point(&view, vcx), Some((1.5, false)));
        seek_to(20, vcx);
        assert_eq!(first_point(&view, vcx), Some((2.5, false)));
    }

    #[gpui::test]
    fn an_out_of_step_overlay_is_flagged(cx: &mut gpui::TestAppContext) {
        let (_temp, view, vcx) = truth_view(&[(5, 1.5), (20, 2.5)], cx);
        seek_to(10, vcx);
        assert_eq!(first_point(&view, vcx), Some((1.5, true)));
        seek_to(20, vcx);
        assert_eq!(first_point(&view, vcx), Some((2.5, false)));
    }

    /// A 16×16 ramp (`x + 16y`) at t = 1 with manual levels 0..255, so a
    /// painted gray is the raw value; and a truth record with one star at
    /// `(u, u)`.
    fn star_on_ramp(
        u: f32,
        cx: &mut gpui::TestAppContext,
    ) -> (tempfile::TempDir, Entity<ImageView>, &mut VisualTestContext) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
        install_truth(&db, &[(1, u)]);
        cx.update(|cx| {
            TemporalController::init(db.clone(), cx);
            dispatch(TimeAction::Seek(TimeExpr::fixed(Timestamp(1))), cx).unwrap();
        });
        let bytes: Vec<u8> = (0..256u16).flat_map(|v| v.to_le_bytes()).collect();
        let layout = FrameLayout::from_component(
            &metor_db::ComponentSchema::new(metor_proto::types::PrimType::U16, &[16usize, 16][..]),
            &metor_proto_wkt::ComponentMetadata::from("cam")
                .with_image(metor_proto_wkt::ImageFormat::Mono16, 12),
        )
        .unwrap();
        let frame = Frame {
            timestamp: Timestamp(1),
            bytes: bytes.into(),
            layout,
        };
        let display = Display {
            levels: Levels::Manual {
                black: 0.0,
                white: 255.0,
            },
            ..Display::default()
        };
        let source = StaticSource(FrameState::Ready {
            frame,
            stale: false,
        });
        let (_host, view, vcx) = host(
            move |cx| {
                let mut view = ImageView::with_source(Box::new(source), display, db, cx);
                let truth = OverlayConfig {
                    size: None,
                    cat_id: None,
                    ..OverlayConfig::from_root("cam.truth", OverlayColor::Truth)
                };
                view.add_overlay(truth, cx);
                view
            },
            cx,
        );
        (temp, view, vcx)
    }

    #[gpui::test]
    fn an_overlay_point_sits_on_its_painted_pixel(cx: &mut gpui::TestAppContext) {
        let (_temp, view, vcx) = star_on_ramp(10.5, cx);
        vcx.update(|_, cx| {
            view.update(cx, |view, cx| {
                // Ten device pixels per source pixel, about a fractional centre.
                let start = Viewport::centered((7.3, 8.6), 0.1, view.out);
                view.set_free(start, (16, 16), cx);
            })
        });
        draw(vcx);
        let (readout, star, out, image) = vcx.update(|_, cx| {
            view.update(cx, |view, _| {
                let (viewport, _) = view.current_viewport().unwrap();
                view.hover = Some(view.window_point(&viewport, (10.5, 10.5)));
            });
            let view = view.read(cx);
            let (_, points, _) = view.overlay_points(cx).into_iter().next().expect("points");
            let (viewport, _) = view.current_viewport().unwrap();
            let star = points.stars[0];
            let at = view.window_point(&viewport, (f64::from(star.u), f64::from(star.v)));
            let (image, out) = view.frames.current.clone().expect("an image");
            (view.readout(cx), view.device(at), out, image)
        });
        let readout = readout.expect("a readout");
        assert_eq!((readout.x, readout.y), (10, 10));
        assert_eq!(readout.value, "170");
        // The star's device x is the middle of the painted run of pixel 170.
        let row = star.1.floor() as usize - out.y;
        let bytes = image.as_bytes(0).unwrap();
        let run: Vec<usize> = (0..out.width)
            .filter(|&c| bytes[(row * out.width + c) * 4 + 2] == 170)
            .collect();
        assert!(!run.is_empty(), "pixel 10 is painted on the star's row");
        let middle = (run[0] + run[run.len() - 1] + 1) as f64 / 2.0 + out.x as f64;
        assert!(
            (star.0 - middle).abs() <= 0.5,
            "star {} middle {middle}",
            star.0
        );
    }

    /// The overlay strip's bounds, after turning the overlay on.
    fn show_overlay(
        view: &Entity<ImageView>,
        vcx: &mut VisualTestContext,
    ) -> Entity<HistogramStrip> {
        vcx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.histogram_overlay = true;
                cx.notify();
            })
        });
        draw(vcx);
        draw(vcx);
        vcx.update(|_, cx| {
            view.read(cx)
                .overlay_strip
                .clone()
                .expect("an overlay strip")
        })
    }

    #[gpui::test]
    fn the_overlay_is_off_by_default_and_toggles(cx: &mut gpui::TestAppContext) {
        let (_temp, view, vcx) = ramp_view(cx);
        let off = vcx.update(|_, cx| view.read(cx).bounds);
        assert!(vcx.update(|_, cx| view.read(cx).overlay_strip.is_none()));
        let strip = show_overlay(&view, vcx);
        let (on, strip_bounds) =
            vcx.update(|_, cx| (view.read(cx).bounds, strip.read(cx).bounds()));
        assert_eq!(f32::from(strip_bounds.size.height), strip::STRIP_HEIGHT);
        assert_eq!(on.size.height, off.size.height - px(strip::STRIP_HEIGHT));
        vcx.update(|_, cx| {
            view.update(cx, |view, cx| {
                view.histogram_overlay = false;
                cx.notify();
            })
        });
        draw(vcx);
        assert!(vcx.update(|_, cx| view.read(cx).overlay_strip.is_none()));
        assert_eq!(vcx.update(|_, cx| view.read(cx).bounds), off);
    }

    #[gpui::test]
    fn a_live_strip_keeps_the_histogram_under_manual_levels(cx: &mut gpui::TestAppContext) {
        let (_temp, view, vcx) = ramp_view(cx);
        vcx.update(|_, cx| view.update(cx, |view, cx| view.set_levels((0.0, 40.0), cx)));
        draw(vcx);
        assert!(vcx.update(|_, cx| view.read(cx).histogram_data.is_none()));
        let strip = vcx.update(|_, cx| ImageView::histogram_strip(&view, cx));
        draw(vcx);
        assert!(vcx.update(|_, cx| view.read(cx).strip_data().is_some()));
        drop(strip);
        draw(vcx);
        draw(vcx);
        assert!(vcx.update(|_, cx| view.read(cx).histogram_data.is_none()));
    }

    #[gpui::test]
    fn dragging_a_strip_handle_sets_manual_levels(cx: &mut gpui::TestAppContext) {
        let (_temp, view, vcx) = ramp_view(cx);
        let strip = show_overlay(&view, vcx);
        let (bounds, levels, range) = vcx.update(|_, cx| {
            let range = view
                .read(cx)
                .histogram_data
                .as_ref()
                .expect("a histogram")
                .range();
            (
                strip.read(cx).bounds(),
                view.read(cx).levels.expect("levels"),
                range,
            )
        });
        assert!(f32::from(bounds.size.width) > 0.0);
        let width = f32::from(bounds.size.width);
        let y = bounds.origin.y + px(10.0);
        let at = |value: f64| bounds.origin.x + px(strip::x_of(value, range, width));
        let white = point(at(levels.1), y);
        let target = point(at(range.0 + (range.1 - range.0) / 2.0), y);
        vcx.simulate_mouse_down(white, MouseButton::Left, gpui::Modifiers::none());
        vcx.simulate_mouse_move(target, MouseButton::Left, gpui::Modifiers::none());
        vcx.simulate_mouse_up(target, MouseButton::Left, gpui::Modifiers::none());
        draw(vcx);
        let display = vcx.update(|_, cx| view.read(cx).display);
        let Levels::Manual { black, white } = display.levels else {
            panic!("manual levels, not {:?}", display.levels);
        };
        assert_eq!(black, levels.0);
        assert!(
            (white - (range.0 + range.1) / 2.0).abs()
                <= (range.1 - range.0) / f64::from(width) * 2.0
        );
    }

    #[gpui::test]
    fn a_handle_drag_released_over_the_image_ends(cx: &mut gpui::TestAppContext) {
        let (_temp, view, vcx) = ramp_view(cx);
        let strip = show_overlay(&view, vcx);
        let bounds = vcx.update(|_, cx| strip.read(cx).bounds());
        let (levels, range, image_point) = vcx.update(|_, cx| {
            let view = view.read(cx);
            let (viewport, _) = view.current_viewport().unwrap();
            (
                view.levels.unwrap(),
                view.histogram_data.as_ref().unwrap().range(),
                view.window_point(&viewport, (4.0, 4.0)),
            )
        });
        let width = f32::from(bounds.size.width);
        let black = point(
            bounds.origin.x + px(strip::x_of(levels.0, range, width)),
            bounds.origin.y + px(10.0),
        );
        vcx.simulate_mouse_down(black, MouseButton::Left, gpui::Modifiers::none());
        vcx.simulate_mouse_move(image_point, MouseButton::Left, gpui::Modifiers::none());
        vcx.simulate_mouse_up(image_point, MouseButton::Left, gpui::Modifiers::none());
        draw(vcx);
        let (dragging, display) = vcx.update(|_, cx| {
            (
                view.read(cx).is_dragging() || strip.read(cx).is_dragging(),
                view.read(cx).display,
            )
        });
        assert!(!dragging);
        let Levels::Manual { black, white } = display.levels else {
            panic!("manual levels");
        };
        assert!(white > black, "{black} {white}");
        // Later moves no longer drag.
        vcx.simulate_mouse_move(strip_corner(bounds), None, gpui::Modifiers::none());
        assert_eq!(vcx.update(|_, cx| view.read(cx).display), display);
    }

    fn strip_corner(bounds: Bounds<Pixels>) -> Point<Pixels> {
        point(bounds.origin.x + px(1.0), bounds.origin.y + px(1.0))
    }

    #[gpui::test]
    fn an_overlay_with_fields_from_different_times_is_out_of_step(cx: &mut gpui::TestAppContext) {
        let (_temp, view, vcx) = truth_view(&[(10, 1.5)], cx);
        seek_to(10, vcx);
        assert_eq!(first_point(&view, vcx), Some((1.5, false)));
        // A newer `u` alone: the record is torn until `v` and `count` follow.
        let db = vcx.update(|_, cx| view.read(cx).db.clone());
        let u = db
            .with_state(|s| s.get_component(ComponentId::new("cam.truth.u")).cloned())
            .unwrap();
        let bytes: Vec<u8> = (0..32)
            .flat_map(|i| if i == 0 { 3.5f32 } else { 0.0 }.to_le_bytes())
            .collect();
        let mut writer = u.time_series.writer().unwrap();
        writer.push_buf(Timestamp(12), &bytes).unwrap();
        seek_to(12, vcx);
        assert_eq!(first_point(&view, vcx), Some((3.5, true)));
    }

    #[test]
    fn config_round_trips() {
        let cfg = ImageViewConfig {
            component: "cam.pixels".into(),
            display: Some(Display::default()),
            view: ViewMode::Fit,
            histogram_overlay: true,
            crosshair: true,
            overlays: vec![OverlayConfig::from_root(
                "sim.star_truth",
                OverlayColor::Truth,
            )],
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert_eq!(serde_json::from_str::<ImageViewConfig>(&json).unwrap(), cfg);
        assert_eq!(
            serde_json::from_str::<ImageViewConfig>("{}").unwrap(),
            ImageViewConfig::default()
        );
    }

    /// The old `histogram` key was on by default and always saved, so it
    /// carries no choice: an old layout opens without the overlay.
    #[test]
    fn a_legacy_histogram_key_leaves_the_overlay_off() {
        let cfg: ImageViewConfig =
            serde_json::from_str(r#"{"component":"cam.pixels","histogram":true}"#).unwrap();
        assert!(!cfg.histogram_overlay);
        assert_eq!(cfg.component, "cam.pixels");
    }
}
