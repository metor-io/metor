//! Free-form dashboard with pixel-positioned widgets.
//!
//! Submodules:
//! - [`widgets`] — per-kind factories and leaf renderers (image, monitor).
//! - [`interaction`] — drag and resize payloads plus the per-widget layout.
//! - [`chrome`] — grid overlay rendered above widgets in edit mode.
//! - [`zoom`] — the canvas-to-screen mapping and zoomed widget content.
use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    App, Bounds, Context, Entity, IntoElement, Pixels, Point, Render, SharedString, Size, Window,
    div, point, prelude::*, px, size,
};
use metor_db::DB;
use serde::{Deserialize, Serialize};

use crate::inspector::rows::{
    BoolRow, CommandRow, DefaultActionRow, InspectorRow, NavRow, ScalarRow,
};
use crate::theme::theme;
use crate::views::Scrollbar;
use crate::views::add_menu::{AddMenu, Host};

use crate::tiles::PaneItem;

mod chrome;
mod interaction;
mod widgets;
mod zoom;

use widgets::create_widget_view;
pub use widgets::{ImageWidgetConfig, TileViewSpec, WidgetLive, WidgetRegistry, WidgetSpec};

const SNAP_GRID_PX: f32 = 10.0;

fn snap_px(v: f32) -> f32 {
    (v / SNAP_GRID_PX).round() * SNAP_GRID_PX
}

/// Monotonic id assigned to a widget on placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WidgetId(pub u64);

/// Pixel rectangle within the dashboard canvas.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WidgetRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Content type of a widget, serialized as a plain string.
///
/// Exposed as a newtype over [`SharedString`] so downstream code can
/// register additional kinds beyond the shipped built-ins via
/// [`WidgetKind::new`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WidgetKind(pub SharedString);

impl WidgetKind {
    pub fn new(name: impl Into<SharedString>) -> Self {
        Self(name.into())
    }
    pub fn plot() -> Self {
        Self(SharedString::new_static("plot"))
    }
    /// The retired Component Text widget's id, which saved dashboards still
    /// carry; it builds a monitor.
    pub fn text() -> Self {
        Self(SharedString::new_static("text"))
    }
    pub fn table() -> Self {
        Self(SharedString::new_static("table"))
    }
    pub fn image() -> Self {
        Self(SharedString::new_static("image"))
    }
    pub fn monitor() -> Self {
        Self(SharedString::new_static("monitor"))
    }
    pub fn viewer3d() -> Self {
        Self(SharedString::new_static("viewer3d"))
    }
    pub fn traffic_light() -> Self {
        Self(SharedString::new_static("traffic_light"))
    }
    pub fn annunciator() -> Self {
        Self(SharedString::new_static("annunciator"))
    }
    /// The annunciator's pre-rename on-disk id, kept so saved dashboards and
    /// target-shipped presets keep resolving.
    pub fn traffic_light_grid() -> Self {
        Self(SharedString::new_static("traffic_light_grid"))
    }
    pub fn meter() -> Self {
        Self(SharedString::new_static("meter"))
    }
    pub fn gauge() -> Self {
        Self(SharedString::new_static("gauge"))
    }
    pub fn state_chip() -> Self {
        Self(SharedString::new_static("state_chip"))
    }
    pub fn label() -> Self {
        Self(SharedString::new_static("label"))
    }
    pub fn state_image() -> Self {
        Self(SharedString::new_static("state_image"))
    }
    pub fn sequence_control() -> Self {
        Self(SharedString::new_static("sequence_control"))
    }
    pub fn attitude() -> Self {
        Self(SharedString::new_static("attitude"))
    }
    pub fn map() -> Self {
        Self(SharedString::new_static("map"))
    }
    pub fn samples_table() -> Self {
        Self(SharedString::new_static("samples_table"))
    }
    /// A live image component; `image` is the static picture widget.
    pub fn image_view() -> Self {
        Self(SharedString::new_static("image_view"))
    }

    fn default_size(&self, cx: &App) -> (f32, f32) {
        widgets::widget_spec(self, cx).default_size
    }
}

/// One placed widget (position + persisted config).
///
/// `config` is an opaque blob whose shape is owned by the
/// [`WidgetKind`]; each kind's builder parses it via `serde_json::from_str`
/// (or any other format it chooses) into a kind-specific config struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardWidget {
    pub id: WidgetId,
    pub rect: WidgetRect,
    pub kind: WidgetKind,
    pub config: String,
    /// Background and outline around the widget. Off, the content sits
    /// directly on the canvas — the P&ID look, where a readout belongs to the
    /// diagram rather than to a card.
    #[serde(default = "default_frame")]
    pub frame: bool,
}

fn default_frame() -> bool {
    true
}

/// Canvas panel that lays out [`DashboardWidget`]s at absolute pixel
/// coordinates.
///
/// Live widget handles are kept outside the persisted document.
pub struct DashboardPanel {
    db: Arc<DB>,
    title: SharedString,
    widgets: Vec<DashboardWidget>,
    widget_live: HashMap<WidgetId, WidgetLive>,

    next_id: u64,
    editing: bool,
    selected: Option<WidgetId>,
    container_bounds: Option<Bounds<Pixels>>,
    /// Pan and zoom. Only the zoom is persisted; a reopened dashboard starts
    /// at its top-left.
    viewport: zoom::Viewport,
}

impl DashboardPanel {
    pub fn new(db: Arc<DB>, _cx: &mut Context<Self>) -> Self {
        Self {
            db,
            title: "Dashboard".into(),
            widgets: Vec::new(),
            widget_live: HashMap::new(),
            next_id: 1,
            editing: false,
            selected: None,
            container_bounds: None,
            viewport: zoom::Viewport::default(),
        }
    }

    fn alloc_id(&mut self) -> WidgetId {
        let id = WidgetId(self.next_id);
        self.next_id += 1;
        id
    }

    /// Widgets paired with their inspectable entity and a palette label.
    ///
    /// Consumed by the palette's `Widget` category so each widget is
    /// reachable without opening the dashboard first.
    pub fn inspectable_widgets(&self, cx: &App) -> Vec<(WidgetId, gpui::AnyEntity, SharedString)> {
        self.widgets
            .iter()
            .filter_map(|w| {
                let live = self.widget_live.get(&w.id)?;
                Some((w.id, live.inspect.clone(), widget_display_label(w, cx)))
            })
            .collect()
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    /// Pick a starting rect for a new widget that sits below existing ones.
    fn auto_place(&self, w: f32, h: f32) -> WidgetRect {
        let x = 20.0_f32;
        let mut y = 20.0_f32;
        for widget in &self.widgets {
            let bottom = widget.rect.y + widget.rect.h;
            if bottom + 10.0 > y {
                y = bottom + 10.0;
            }
        }
        WidgetRect {
            x,
            y: snap_px(y),
            w: snap_px(w),
            h: snap_px(h),
        }
    }

    /// Add a registered kind from an opaque persisted config blob.
    pub fn add_widget(
        &mut self,
        kind: WidgetKind,
        config: String,
        cx: &mut Context<Self>,
    ) -> WidgetId {
        let id = self.alloc_id();
        let (w, h) = kind.default_size(cx);
        let rect = self.auto_place(w, h);
        let live = create_widget_view(&kind, &config, &self.db, cx);
        let widget = DashboardWidget {
            id,
            rect,
            kind,
            config,
            frame: true,
        };
        self.widgets.push(widget);
        self.widget_live.insert(id, live);
        cx.notify();
        id
    }

    fn remove_widget(&mut self, id: WidgetId, cx: &mut Context<Self>) {
        self.widgets.retain(|w| w.id != id);
        // Dropping the live handles here is what cancels the widget's stream
        // tasks; a leftover entry keeps them alive for the dashboard's
        // whole lifetime.
        self.widget_live.remove(&id);
        if self.selected == Some(id) {
            self.selected = None;
        }
        cx.notify();
    }

    fn bring_to_front(&mut self, id: WidgetId, cx: &mut Context<Self>) {
        if let Some(ix) = self.widgets.iter().position(|w| w.id == id) {
            let widget = self.widgets.remove(ix);
            self.widgets.push(widget);
            cx.notify();
        }
    }

    fn send_to_back(&mut self, id: WidgetId, cx: &mut Context<Self>) {
        if let Some(ix) = self.widgets.iter().position(|w| w.id == id) {
            let widget = self.widgets.remove(ix);
            self.widgets.insert(0, widget);
            cx.notify();
        }
    }

    /// Right-click on a widget in edit mode: its [`widget_rows`] as a popup.
    fn open_widget_inspector(
        &self,
        widget_id: WidgetId,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = crate::inspector::open_inspector(cx) else {
            return;
        };
        let Some(rows) = widget_rows(&cx.entity(), widget_id, cx) else {
            return;
        };
        open(
            crate::inspector::InspectorRequest {
                rows,
                mode: crate::inspector::InspectorMode::Anchored(position),
            },
            window,
            cx,
        );
    }

    fn ensure_views(&mut self, cx: &mut Context<Self>) {
        for widget in &self.widgets {
            if !self.widget_live.contains_key(&widget.id) {
                let live = create_widget_view(&widget.kind, &widget.config, &self.db, cx);
                self.widget_live.insert(widget.id, live);
            }
        }
    }

    /// Farthest right and bottom edges across all widgets; bounds the scroll.
    fn content_extent(&self) -> Point<f32> {
        let max_x = self
            .widgets
            .iter()
            .map(|w| w.rect.x + w.rect.w)
            .fold(0.0_f32, f32::max);
        let max_y = self
            .widgets
            .iter()
            .map(|w| w.rect.y + w.rect.h)
            .fold(0.0_f32, f32::max);
        point(max_x, max_y)
    }

    /// Size of the on-screen canvas, once it has been laid out.
    fn view_size(&self) -> Option<Size<f32>> {
        self.container_bounds
            .map(|b| size(f32::from(b.size.width), f32::from(b.size.height)))
    }

    /// Keep the pan inside the reachable range given the current viewport.
    fn clamp_scroll(&mut self) {
        let extent = self.content_extent();
        self.viewport.clamp(extent, self.view_size());
    }

    /// Zoom to `zoom` (clamped) around a window-space point, or around the
    /// viewport's centre for inspector commands, which have no cursor to
    /// anchor to.
    fn zoom_to(&mut self, zoom: f32, anchor: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        let anchor = match anchor {
            Some(at) => self.window_to_local(at),
            None => self
                .view_size()
                .map(|v| point(v.width / 2.0, v.height / 2.0)),
        }
        .unwrap_or(point(0.0, 0.0));
        self.viewport.zoom_around(zoom, anchor);
        self.clamp_scroll();
        cx.notify();
    }

    /// Zoom out until every widget is on screen at once.
    fn fit_all(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.view_size() else {
            return;
        };
        self.viewport = zoom::Viewport::fit(self.content_extent(), view);
        cx.notify();
    }

    /// A window-space point relative to the canvas container, before pan
    /// and zoom.
    fn window_to_local(&self, pixel: Point<Pixels>) -> Option<Point<f32>> {
        let bounds = self.container_bounds?;
        Some(point(
            f32::from(pixel.x - bounds.origin.x),
            f32::from(pixel.y - bounds.origin.y),
        ))
    }

    /// Translate a window-space point into canvas coordinates, undoing the
    /// current pan and zoom.
    fn pixel_to_canvas(&self, pixel: Point<Pixels>) -> Option<Point<f32>> {
        Some(self.viewport.to_canvas(self.window_to_local(pixel)?))
    }
}

/// A widget's reflected settings page, plus the rows that belong to its
/// placement on the dashboard rather than to the view itself. Shared by the
/// right-click popup and the inspector pane. `None` once the widget is gone.
pub fn widget_rows(
    dashboard: &Entity<DashboardPanel>,
    widget_id: WidgetId,
    cx: &App,
) -> Option<Vec<Box<dyn InspectorRow>>> {
    let this = dashboard.read(cx);
    let live = this.widget_live.get(&widget_id)?;
    let mut rows = crate::inspector::reflect::rows_for_any_entity(&live.inspect, &this.db, cx)
        .unwrap_or_default();
    rows.push(Box::new(BoolRow::dynamic(
        "Frame",
        Arc::new({
            let dashboard = dashboard.clone();
            move |cx| {
                dashboard
                    .read(cx)
                    .widgets
                    .iter()
                    .find(|w| w.id == widget_id)
                    .is_none_or(|w| w.frame)
            }
        }),
        Arc::new({
            let dashboard = dashboard.clone();
            move |on, _window, cx| {
                dashboard.update(cx, |this, cx| {
                    if let Some(w) = this.widgets.iter_mut().find(|w| w.id == widget_id) {
                        w.frame = on;
                        cx.notify();
                    }
                });
            }
        }),
    )));
    Some(rows)
}

/// Inspector rows that operate on the dashboard itself: add/remove widgets,
/// toggle edit mode, rename, adjust z-order.
pub fn dashboard_rows(
    dashboard: Entity<DashboardPanel>,
    db: Arc<DB>,
    cx: &App,
) -> Vec<Box<dyn InspectorRow>> {
    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();

    rows.push(Box::new(NavRow::new(
        "Add Widget",
        SharedString::new_static(""),
        {
            let dashboard = dashboard.clone();
            let db = db.clone();
            Box::new(move |_| add_widget_rows(dashboard.clone(), db.clone()))
        },
    )));

    let editing = dashboard.read(cx).editing;
    let edit_label: SharedString = if editing {
        "Exit Edit Mode".into()
    } else {
        "Enter Edit Mode".into()
    };
    rows.push(Box::new(CommandRow::new(edit_label, {
        let dashboard = dashboard.clone();
        Arc::new(move |_window, cx| {
            dashboard.update(cx, |this, cx| {
                this.editing = !this.editing;
                if !this.editing {
                    this.selected = None;
                }
                cx.notify();
            });
        })
    })));

    let widgets = &dashboard.read(cx).widgets;
    if !widgets.is_empty() {
        let widget_infos: Vec<(WidgetId, SharedString)> = widgets
            .iter()
            .map(|w| (w.id, widget_display_label(w, cx)))
            .collect();
        rows.push(Box::new(NavRow::new(
            "Remove Widget",
            SharedString::new_static(""),
            {
                let dashboard = dashboard.clone();
                Box::new(move |_cx| {
                    widget_infos
                        .iter()
                        .map(|(widget_id, label)| {
                            let widget_id = *widget_id;
                            let dashboard = dashboard.clone();
                            Box::new(CommandRow::new(
                                label.clone(),
                                Arc::new(move |_window, cx| {
                                    dashboard.update(cx, |this, cx| {
                                        this.remove_widget(widget_id, cx);
                                    });
                                }),
                            )) as Box<dyn InspectorRow>
                        })
                        .collect()
                })
            },
        )));
    }

    let selected = dashboard.read(cx).selected;
    if let Some(sel_id) = selected {
        rows.push(Box::new(CommandRow::new("Bring to Front", {
            let dashboard = dashboard.clone();
            Arc::new(move |_window, cx| {
                dashboard.update(cx, |this, cx| {
                    this.bring_to_front(sel_id, cx);
                });
            })
        })));
        rows.push(Box::new(CommandRow::new("Send to Back", {
            let dashboard = dashboard.clone();
            Arc::new(move |_window, cx| {
                dashboard.update(cx, |this, cx| {
                    this.send_to_back(sel_id, cx);
                });
            })
        })));
    }

    let zoom = dashboard.read(cx).viewport.zoom;
    rows.push(Box::new(NavRow::new(
        "Zoom",
        SharedString::from(format!("{:.0}%", zoom * 100.0)),
        {
            let dashboard = dashboard.clone();
            Box::new(move |cx| zoom_rows(dashboard.clone(), cx))
        },
    )));

    rows.push(Box::new(NavRow::new(
        "Rename",
        SharedString::new_static(""),
        {
            let dashboard = dashboard.clone();
            Box::new(move |_cx| {
                let dashboard = dashboard.clone();
                vec![Box::new(DefaultActionRow::new(
                    "Dashboard name...",
                    Arc::new(move |input, _window, cx| {
                        if !input.is_empty() {
                            dashboard.update(cx, |this, cx| {
                                this.title = SharedString::from(input);
                                cx.notify();
                            });
                        }
                    }),
                )) as Box<dyn InspectorRow>]
            })
        },
    )));

    rows
}

/// Zoom commands for the dashboard's inspector page. Steps zoom around the
/// viewport's centre; the wheel and pinch gestures anchor at the cursor.
fn zoom_rows(dashboard: Entity<DashboardPanel>, cx: &App) -> Vec<Box<dyn InspectorRow>> {
    let step = |label: &'static str, zoom: fn(f32) -> f32| {
        let dashboard = dashboard.clone();
        Box::new(CommandRow::new(
            label,
            Arc::new(move |_window, cx| {
                dashboard.update(cx, |this, cx| {
                    this.zoom_to(zoom(this.viewport.zoom), None, cx);
                });
            }),
        )) as Box<dyn InspectorRow>
    };
    let percent = {
        let dashboard = dashboard.clone();
        ScalarRow::new(
            "Zoom %".into(),
            f64::from(dashboard.read(cx).viewport.zoom * 100.0).round(),
            Arc::new(move |value, _window, cx| {
                dashboard.update(cx, |this, cx| this.zoom_to(value as f32 / 100.0, None, cx));
            }),
        )
    };
    let fit = {
        let dashboard = dashboard.clone();
        CommandRow::new(
            "Fit All",
            Arc::new(move |_window, cx| dashboard.update(cx, |this, cx| this.fit_all(cx))),
        )
    };
    vec![
        Box::new(fit),
        step("Zoom In", |z| z * zoom::ZOOM_STEP),
        step("Zoom Out", |z| z / zoom::ZOOM_STEP),
        step("Actual Size", |_| 1.0),
        Box::new(percent),
    ]
}

/// The shared "add view" menu aimed at `dashboard`.
fn add_widget_rows(dashboard: Entity<DashboardPanel>, db: Arc<DB>) -> Vec<Box<dyn InspectorRow>> {
    AddMenu {
        db,
        host: Host::Dashboard,
        sink: Arc::new(move |kind, config, cx| {
            dashboard.update(cx, |this, cx| {
                let id = this.add_widget(kind.clone(), config, cx);
                this.widget_live.get(&id).map(|live| live.inspect.clone())
            })
        }),
        on_open_inspector: None,
    }
    .rows()
}

fn widget_display_label(widget: &DashboardWidget, cx: &App) -> SharedString {
    (widgets::widget_spec(&widget.kind, cx).label)(widget)
}

impl Render for DashboardPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_views(cx);
        let theme = theme(cx);

        let entity = cx.entity().clone();
        let zoom_entity = entity.clone();
        let bounds_tracker = gpui::canvas(
            move |bounds, window, cx| {
                entity.update(cx, |this, _| {
                    this.container_bounds = Some(bounds);
                });
                window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal)
            },
            move |_, hitbox, window, _| {
                // Zoom gestures are taken in the capture phase: plots and maps
                // stop wheel propagation for their own zoom, and a modified
                // wheel over one should still zoom the dashboard.
                let entity = zoom_entity.clone();
                let wheel_hitbox = hitbox.clone();
                window.on_mouse_event(move |event: &gpui::ScrollWheelEvent, phase, window, cx| {
                    let modified = event.modifiers.platform || event.modifiers.control;
                    if phase != gpui::DispatchPhase::Capture
                        || !modified
                        || !wheel_hitbox.should_handle_scroll(window)
                    {
                        return;
                    }
                    cx.stop_propagation();
                    let dy = f32::from(event.delta.pixel_delta(px(20.0)).y);
                    entity.update(cx, |this, cx| {
                        let zoom = this.viewport.zoom * zoom::wheel_zoom_factor(dy);
                        this.zoom_to(zoom, Some(event.position), cx);
                    });
                });
                let entity = zoom_entity.clone();
                window.on_mouse_event(move |event: &gpui::PinchEvent, phase, window, cx| {
                    if phase != gpui::DispatchPhase::Capture || !hitbox.is_hovered(window) {
                        return;
                    }
                    cx.stop_propagation();
                    entity.update(cx, |this, cx| {
                        let zoom = this.viewport.zoom * (1.0 + event.delta);
                        this.zoom_to(zoom, Some(event.position), cx);
                    });
                });
            },
        )
        .size_full()
        .absolute();

        let mut canvas_div = div()
            .id("dashboard-canvas")
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(theme.bg_secondary)
            .child(bounds_tracker);

        if self.editing {
            canvas_div = canvas_div.child(self.render_grid_overlay(cx));
        }

        // Vec order is z-order: later widgets paint on top.
        let widgets: Vec<DashboardWidget> = self.widgets.clone();
        for widget in &widgets {
            canvas_div = canvas_div.child(self.render_widget(widget, cx));
        }

        canvas_div = canvas_div
            .on_drag_move(cx.listener(Self::handle_widget_drag_move))
            .on_drag_move(cx.listener(Self::handle_widget_resize_move));

        // Wheel events pan the canvas; widgets that consume wheel (plots)
        // must stop propagation to keep their own handling authoritative.
        canvas_div = canvas_div.on_scroll_wheel(cx.listener(
            |this, event: &gpui::ScrollWheelEvent, _window, cx| {
                let delta = event.delta.pixel_delta(px(20.0));
                this.viewport.scroll.x += f32::from(delta.x);
                this.viewport.scroll.y += f32::from(delta.y);
                this.clamp_scroll();
                cx.notify();
            },
        ));

        if self.editing {
            let entity = cx.entity();
            canvas_div = canvas_div.on_click(move |_, _, cx| {
                entity.update(cx, |this, cx| {
                    this.selected = None;
                    cx.notify();
                });
            });
        }

        if self.editing {
            canvas_div = canvas_div.child(
                div()
                    .absolute()
                    .top(px(4.0))
                    .right(px(4.0))
                    .px(px(8.0))
                    .py(px(2.0))
                    .bg(theme.pill_bg)
                    .border_1()
                    .border_color(theme.pill_border)
                    .rounded(px(4.0))
                    .text_size(px(11.0))
                    .text_color(theme.text_secondary)
                    .child("EDIT MODE"),
            );
        }

        if self.widgets.is_empty() {
            canvas_div = canvas_div.child(
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(theme.text_tertiary)
                    .text_size(px(14.0))
                    .child("Open command palette to add widgets"),
            );
        }

        if let Some(view) = self.view_size() {
            let extent = self.content_extent();
            let zoom = self.viewport.zoom;
            let scroll = self.viewport.scroll;

            canvas_div = canvas_div
                .child(Scrollbar::new(
                    gpui::Axis::Vertical,
                    view.height,
                    extent.y * zoom,
                    -scroll.y,
                ))
                .child(Scrollbar::new(
                    gpui::Axis::Horizontal,
                    view.width,
                    extent.x * zoom,
                    -scroll.x,
                ));
        }

        canvas_div
    }
}

/// Persisted shape of [`DashboardPanel`].
///
/// Every field defaults, so a document written before a field existed
/// loads as if it had been saved with that default.
#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct DashboardPanelConfig {
    pub title: String,
    pub widgets: Vec<DashboardWidget>,
    /// Screen pixels per dashboard unit. Widget rects never change with it.
    pub zoom: f32,
}

impl Default for DashboardPanelConfig {
    fn default() -> Self {
        Self {
            title: String::new(),
            widgets: Vec::new(),
            zoom: 1.0,
        }
    }
}

impl PaneItem for DashboardPanel {
    type Config = DashboardPanelConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn serialization_key() -> &'static str {
        "dashboard"
    }

    fn to_config(&self, cx: &App) -> DashboardPanelConfig {
        // Refresh widget configs from live entities so inspector edits land
        // on disk. Kinds without editable state return `None` and keep
        // their cached blob.
        let widgets = self
            .widgets
            .iter()
            .map(|w| {
                let mut w = w.clone();
                if let Some(live) = self.widget_live.get(&w.id)
                    && let Some(spec) = cx.global::<WidgetRegistry>().spec(&w.kind)
                    && let Some(blob) = (spec.snapshot)(&live.state, &w.config, cx)
                {
                    w.config = blob;
                }
                w
            })
            .collect();
        DashboardPanelConfig {
            title: self.title.to_string(),
            widgets,
            zoom: self.viewport.zoom,
        }
    }
}

fn next_widget_id(widgets: &[DashboardWidget]) -> u64 {
    widgets.iter().map(|w| w.id.0 + 1).max().unwrap_or(1).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dashboards saved before zoom existed open at 1:1, not at the zero a
    /// derived default would give.
    #[test]
    fn a_dashboard_saved_before_zoom_opens_unzoomed() {
        let cfg: DashboardPanelConfig =
            serde_json::from_str(r#"{"title":"Old","widgets":[],"connectors":[]}"#).unwrap();
        assert_eq!(cfg.zoom, 1.0);
        assert_eq!(DashboardPanelConfig::default().zoom, 1.0);
    }

    /// Cross-language pin: the dashboard the Python preset builder emits
    /// (`metor_config.Dashboard`) must parse into this config exactly.
    ///
    /// Both sides read one fixture — `test_golden.py` asserts the recorder
    /// still produces it, this asserts the panel still understands it. The
    /// widget `config` blob is an opaque string to the FSW-side preset
    /// validator, so nothing upstream catches a shape mismatch; without this
    /// pin a rename turns a shipped dashboard into placeholder tiles with no
    /// error anywhere.
    #[test]
    fn the_golden_python_dashboard_parses() {
        let blob = include_str!("../../../../metor-fsw-2/tests/golden/dashboard.json");
        let cfg: DashboardPanelConfig = serde_json::from_str(blob).unwrap();
        assert_eq!(cfg.title, "Golden");

        // Every kind the builder can emit must be one the registry knows; an
        // unlisted kind would render as "? unknown kind".
        let known = [
            WidgetKind::meter(),
            WidgetKind::gauge(),
            WidgetKind::state_chip(),
            WidgetKind::attitude(),
            WidgetKind::sequence_control(),
            WidgetKind::traffic_light(),
            WidgetKind::traffic_light_grid(),
            WidgetKind::monitor(),
            WidgetKind::plot(),
            WidgetKind::image(),
        ];
        for kind in &known {
            assert!(
                cfg.widgets.iter().any(|w| w.kind == *kind),
                "fixture no longer covers {kind:?}"
            );
        }
        for w in &cfg.widgets {
            assert!(known.contains(&w.kind), "unregistered kind {:?}", w.kind);
        }

        let by_kind = |kind: WidgetKind| -> &str {
            &cfg.widgets.iter().find(|w| w.kind == kind).unwrap().config
        };

        // Each widget's opaque blob must parse into that kind's own config.
        let meter: crate::views::MeterConfig =
            serde_json::from_str(by_kind(WidgetKind::meter())).unwrap();
        assert_eq!(meter.component, "sat1.wheels.h");
        assert_eq!((meter.min, meter.max), (-0.04, 0.04));
        assert!(matches!(
            meter.orientation,
            crate::views::Orientation::Vertical
        ));

        let gauge: crate::views::GaugeConfig =
            serde_json::from_str(by_kind(WidgetKind::gauge())).unwrap();
        assert!(matches!(gauge.style, crate::views::GaugeStyle::Needle));
        assert_eq!(gauge.sweep_degrees, 200.0);

        let chip: crate::views::StateChipConfig =
            serde_json::from_str(by_kind(WidgetKind::state_chip())).unwrap();
        assert_eq!(chip.states.len(), 2);
        assert_eq!(chip.states[1].label, "SAFE");
        assert!(chip.states[1].color.is_some());
        assert_eq!(chip.unknown_label, "UNKNOWN");

        let att: crate::views::AttitudeConfig =
            serde_json::from_str(by_kind(WidgetKind::attitude())).unwrap();
        assert_eq!(att.vectors[0].component, "sat1.sensors.mag_b");
        assert_eq!(att.vectors[0].label, "mag");

        let seq: crate::views::SequenceControlConfig =
            serde_json::from_str(by_kind(WidgetKind::sequence_control())).unwrap();
        // A channel is a slot name, never namespace-qualified.
        assert_eq!(seq.channel, "mode");
        assert!(seq.compact);

        // The image travels as bytes, not as a path the panel may not be able
        // to reach.
        let image: widgets::ImageWidgetConfig =
            serde_json::from_str(by_kind(WidgetKind::image())).unwrap();
        assert!(!image.data.is_empty(), "image was not inlined");
    }

    #[gpui::test]
    fn zoom_is_driven_from_the_inspector(cx: &mut gpui::TestAppContext) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
        let cx = cx.add_empty_window();
        let dashboard = cx.update(|_, cx| cx.new(|cx| DashboardPanel::new(db.clone(), cx)));
        let run = |label: &str, cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                let mut rows = zoom_rows(dashboard.clone(), cx);
                let row = rows.iter_mut().find(|r| r.label() == label).unwrap();
                row.activate(window, cx);
            });
            dashboard.read_with(cx, |d, _| d.viewport.zoom)
        };

        assert_eq!(run("Zoom Out", cx), 1.0 / zoom::ZOOM_STEP);
        assert_eq!(run("Zoom In", cx), 1.0);
        run("Zoom Out", cx);
        assert_eq!(run("Actual Size", cx), 1.0);
        let labels: Vec<String> = cx.update(|_, cx| {
            dashboard_rows(dashboard.clone(), db.clone(), cx)
                .iter()
                .map(|r| r.label().to_string())
                .collect()
        });
        assert!(labels.iter().any(|l| l == "Zoom"));
    }
}

/// Rebuild a [`DashboardPanel`] from its persisted JSON blob.
///
/// Returns a freshly defaulted dashboard if the blob fails to parse —
/// preferable to crashing on a stale or hand-edited config file.
#[allow(clippy::items_after_test_module)]
pub fn deserialize_dashboard(db: Arc<DB>, blob: &str, cx: &mut App) -> Entity<DashboardPanel> {
    let cfg: DashboardPanelConfig = serde_json::from_str(blob).unwrap_or_default();

    let mut widget_live = HashMap::new();
    for widget in &cfg.widgets {
        widget_live.insert(
            widget.id,
            create_widget_view(&widget.kind, &widget.config, &db, cx),
        );
    }

    let next_id = next_widget_id(&cfg.widgets);

    cx.new(|_cx| DashboardPanel {
        db,
        title: if cfg.title.is_empty() {
            SharedString::from("Dashboard")
        } else {
            SharedString::from(cfg.title)
        },
        widgets: cfg.widgets,
        widget_live,
        next_id,
        editing: false,
        selected: None,
        container_bounds: None,
        viewport: zoom::Viewport::at_zoom(cfg.zoom),
    })
}
