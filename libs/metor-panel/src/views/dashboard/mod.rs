//! Free-form dashboard with pixel-positioned widgets.
//!
//! Submodules:
//! - [`widgets`] — per-kind factories and leaf renderers (image, monitor).
//! - [`interaction`] — drag and resize payloads plus the per-widget layout.
//! - [`chrome`] — grid overlay rendered above widgets in edit mode.
use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    AnyView, App, Bounds, Context, Entity, IntoElement, Pixels, Point, Render, SharedString,
    Window, div, point, prelude::*, px,
};
use metor_db::DB;

use crate::views::{Scrollbar, TimeSeriesPlot};
use crate::theme::theme;
use crate::inspector::rows::{CommandRow, DefaultActionRow, InspectorRow, NavRow};

use crate::tiles::PaneItem;

mod chrome;
mod interaction;
mod widgets;

pub use widgets::{WidgetRegistry, WidgetSpec};
use widgets::create_widget_view;

const SNAP_GRID_PX: f32 = 10.0;

fn snap_px(v: f32) -> f32 {
    (v / SNAP_GRID_PX).round() * SNAP_GRID_PX
}

/// Monotonic id assigned to a widget on placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, facet::Facet)]
#[facet(transparent)]
pub struct WidgetId(pub u64);

/// Pixel rectangle within the dashboard canvas.
#[derive(Debug, Clone, Copy, facet::Facet)]
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
#[derive(Debug, Clone, PartialEq, Eq, Hash, facet::Facet)]
#[facet(transparent)]
pub struct WidgetKind(pub SharedString);

impl WidgetKind {
    pub fn new(name: impl Into<SharedString>) -> Self {
        Self(name.into())
    }
    pub fn plot() -> Self {
        Self(SharedString::new_static("plot"))
    }
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

    fn default_size(&self, cx: &App) -> (f32, f32) {
        widgets::widget_spec(self, cx).default_size
    }
}

/// One placed widget (position + persisted config).
///
/// `config` is an opaque blob whose shape is owned by the
/// [`WidgetKind`]; each kind's builder parses it via `facet_json::from_str`
/// (or any other format it chooses) into a kind-specific config struct.
#[derive(Debug, Clone, facet::Facet)]
pub struct DashboardWidget {
    pub id: WidgetId,
    pub rect: WidgetRect,
    pub kind: WidgetKind,
    pub config: String,
}

/// Canvas panel that lays out [`DashboardWidget`]s at absolute pixel
/// coordinates.
///
/// Widget entities live in parallel maps keyed by [`WidgetId`] so the
/// rendered view and the inspectable data model stay addressable without
/// holding references inside the config.
pub struct DashboardPanel {
    db: Arc<DB>,
    title: SharedString,
    widgets: Vec<DashboardWidget>,
    widget_views: HashMap<WidgetId, AnyView>,
    widget_entities: HashMap<WidgetId, gpui::AnyEntity>,

    next_id: u64,
    editing: bool,
    selected: Option<WidgetId>,
    container_bounds: Option<Bounds<Pixels>>,
    /// Viewport pan in pixels; negative values scroll content below and
    /// right of origin into view.
    scroll_offset: Point<f32>,
}

impl DashboardPanel {
    pub fn new(db: Arc<DB>, _cx: &mut Context<Self>) -> Self {
        Self {
            db,
            title: "Dashboard".into(),
            widgets: Vec::new(),
            widget_views: HashMap::new(),
            widget_entities: HashMap::new(),
            next_id: 1,
            editing: false,
            selected: None,
            container_bounds: None,
            scroll_offset: point(0.0, 0.0),
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
    pub fn inspectable_widgets(
        &self,
        cx: &App,
    ) -> Vec<(WidgetId, gpui::AnyEntity, SharedString)> {
        self.widgets
            .iter()
            .filter_map(|w| {
                let entity = self.widget_entities.get(&w.id)?.clone();
                Some((w.id, entity, widget_display_label(w, cx)))
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

    /// Add a widget whose view and inspectable entity are already built.
    ///
    /// Used by flows (e.g. the trace wizard) that want to seed a widget
    /// with runtime state. `config` is left empty; the pre-built state is
    /// not round-tripped through serialization.
    pub fn add_widget_with_entity(
        &mut self,
        kind: WidgetKind,
        view: AnyView,
        entity: gpui::AnyEntity,
        cx: &mut Context<Self>,
    ) -> WidgetId {
        let id = self.alloc_id();
        let (w, h) = kind.default_size(cx);
        let rect = self.auto_place(w, h);
        self.widgets.push(DashboardWidget {
            id,
            rect,
            kind,
            config: "{}".to_string(),
        });
        self.widget_views.insert(id, view);
        self.widget_entities.insert(id, entity);
        cx.notify();
        id
    }

    fn add_widget(
        &mut self,
        kind: WidgetKind,
        config: String,
        cx: &mut Context<Self>,
    ) -> WidgetId {
        let id = self.alloc_id();
        let (w, h) = kind.default_size(cx);
        let rect = self.auto_place(w, h);
        let (view, widget_entity) = create_widget_view(&kind, &config, &self.db, cx);
        let widget = DashboardWidget {
            id,
            rect,
            kind,
            config,
        };
        self.widgets.push(widget);
        self.widget_views.insert(id, view);
        self.widget_entities.insert(id, widget_entity);
        cx.notify();
        id
    }

    fn remove_widget(&mut self, id: WidgetId, cx: &mut Context<Self>) {
        self.widgets.retain(|w| w.id != id);
        self.widget_views.remove(&id);
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

    fn open_widget_inspector(
        &self,
        widget_id: WidgetId,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(entity) = self.widget_entities.get(&widget_id) {
            window.dispatch_action(
                Box::new(crate::inspector::InspectEntity {
                    entity: entity.clone(),
                    position,
                }),
                cx,
            );
        }
    }

    fn ensure_views(&mut self, cx: &mut Context<Self>) {
        for widget in &self.widgets {
            if !self.widget_views.contains_key(&widget.id) {
                let (view, widget_entity) =
                    create_widget_view(&widget.kind, &widget.config, &self.db, cx);
                self.widget_views.insert(widget.id, view);
                self.widget_entities.insert(widget.id, widget_entity);
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

    /// Keep `scroll_offset` inside the reachable range given the current viewport.
    fn clamp_scroll(&mut self) {
        let extent = self.content_extent();
        if let Some(bounds) = self.container_bounds {
            let vw = f32::from(bounds.size.width);
            let vh = f32::from(bounds.size.height);
            // scroll_offset is negative (scroll down = more negative)
            let min_x = -(extent.x - vw).max(0.0);
            let min_y = -(extent.y - vh).max(0.0);
            self.scroll_offset.x = self.scroll_offset.x.clamp(min_x, 0.0);
            self.scroll_offset.y = self.scroll_offset.y.clamp(min_y, 0.0);
        } else {
            self.scroll_offset.x = self.scroll_offset.x.min(0.0);
            self.scroll_offset.y = self.scroll_offset.y.min(0.0);
        }
    }

    /// Translate a window-space point into canvas coordinates, applying the
    /// current scroll offset.
    fn pixel_to_canvas(&self, pixel: Point<Pixels>) -> Option<Point<f32>> {
        let bounds = self.container_bounds?;
        let x = f32::from(pixel.x - bounds.origin.x) - self.scroll_offset.x;
        let y = f32::from(pixel.y - bounds.origin.y) - self.scroll_offset.y;
        Some(point(x, y))
    }
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
            Box::new(move |_cx| add_widget_rows(dashboard.clone(), db.clone()))
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
        rows.push(Box::new(NavRow::new("Remove Widget", SharedString::new_static(""), {
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
        })));
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

    rows.push(Box::new(NavRow::new("Rename", SharedString::new_static(""), {
        let dashboard = dashboard.clone();
        Box::new(move |_cx| {
            let dashboard = dashboard.clone();
            vec![Box::new(DefaultActionRow {
                label: "Dashboard name...".into(),
                callback: Arc::new(move |input, _window, cx| {
                    if !input.is_empty() {
                        dashboard.update(cx, |this, cx| {
                            this.title = SharedString::from(input);
                            cx.notify();
                        });
                    }
                }),
            }) as Box<dyn InspectorRow>]
        })
    })));

    rows
}

fn add_widget_rows(
    dashboard: Entity<DashboardPanel>,
    db: Arc<DB>,
) -> Vec<Box<dyn InspectorRow>> {
    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();

    rows.push(Box::new(NavRow::new(
        "Time Series Plot",
        SharedString::new_static(""),
        {
            let dashboard = dashboard.clone();
            let db = db.clone();
            Box::new(move |_cx| {
                let dashboard = dashboard.clone();
                let db_for_plot = db.clone();
                crate::inspector::trace_picker::select_traces_wizard_rows(
                    db.clone(),
                    Arc::new(|_cx| 0),
                    Arc::new(move |traces, _window, cx| {
                        let db_for_plot = db_for_plot.clone();
                        let plot = cx.new(|cx| TimeSeriesPlot::new(db_for_plot, traces, cx));
                        let line_plot = plot.read(cx).line_plot().clone();
                        dashboard.update(cx, |this, cx| {
                            this.add_widget_with_entity(
                                WidgetKind::plot(),
                                AnyView::from(plot),
                                line_plot.into_any(),
                                cx,
                            );
                        });
                    }),
                )
            })
        },
    )));
    rows.push(Box::new(NavRow::new("Component Text", SharedString::new_static(""), {
        let dashboard = dashboard.clone();
        let db = db.clone();
        Box::new(move |_cx| {
            component_picker_rows(dashboard.clone(), db.clone(), WidgetKind::text())
        })
    })));
    rows.push(Box::new(CommandRow::new("Component Table", {
        let dashboard = dashboard.clone();
        Arc::new(move |_window, cx| {
            dashboard.update(cx, |this, cx| {
                this.add_widget(WidgetKind::table(), "{}".to_string(), cx);
            });
        })
    })));
    rows.push(Box::new(NavRow::new("Monitor", SharedString::new_static(""), {
        let dashboard = dashboard.clone();
        let db = db.clone();
        Box::new(move |_cx| {
            component_picker_rows(dashboard.clone(), db.clone(), WidgetKind::monitor())
        })
    })));
    rows.push(Box::new(NavRow::new("Image", SharedString::new_static(""), {
        let dashboard = dashboard.clone();
        Box::new(move |_cx| image_path_rows(dashboard.clone()))
    })));
    rows.push(Box::new(CommandRow::new("3D Viewer", {
        let dashboard = dashboard.clone();
        Arc::new(move |_window, cx| {
            dashboard.update(cx, |this, cx| {
                this.add_widget(WidgetKind::viewer3d(), "{}".to_string(), cx);
            });
        })
    })));

    rows
}

fn component_picker_rows(
    dashboard: Entity<DashboardPanel>,
    db: Arc<DB>,
    kind: WidgetKind,
) -> Vec<Box<dyn InspectorRow>> {
    crate::inspector::trace_picker::list_components(&db)
        .into_iter()
        .map(|(_id, name)| {
            let dashboard = dashboard.clone();
            let name_clone = name.clone();
            let kind = kind.clone();
            Box::new(CommandRow::new(
                SharedString::from(name),
                Arc::new(move |_window, cx| {
                    // Both the text and monitor builders accept the same
                    // `{ component: String }` shape, so a single config type
                    // suffices regardless of which kind the wizard was opened
                    // for.
                    let cfg = widgets::TextWidgetConfig {
                        component: name_clone.clone(),
                    };
                    let config =
                        facet_json::to_string(&cfg).expect("component widget config serializes");
                    let kind = kind.clone();
                    dashboard.update(cx, |this, cx| {
                        this.add_widget(kind, config, cx);
                    });
                }),
            )) as Box<dyn InspectorRow>
        })
        .collect()
}

fn image_path_rows(dashboard: Entity<DashboardPanel>) -> Vec<Box<dyn InspectorRow>> {
    vec![Box::new(DefaultActionRow {
        label: "Image file path...".into(),
        callback: Arc::new(move |input, _window, cx| {
            if !input.is_empty() {
                let cfg = widgets::ImageWidgetConfig {
                    path: input.to_string(),
                };
                let config =
                    facet_json::to_string(&cfg).expect("image widget config serializes");
                dashboard.update(cx, |this, cx| {
                    this.add_widget(WidgetKind::image(), config, cx);
                });
            }
        }),
    })]
}

fn widget_display_label(widget: &DashboardWidget, cx: &App) -> SharedString {
    (widgets::widget_spec(&widget.kind, cx).label)(widget)
}

impl Render for DashboardPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_views(cx);
        let theme = theme(cx);

        let entity = cx.entity().clone();
        let bounds_tracker = gpui::canvas(
            move |bounds, _window, cx| {
                entity.update(cx, |this, _| {
                    this.container_bounds = Some(bounds);
                });
            },
            |_, _, _, _| {},
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
                this.scroll_offset.x += f32::from(delta.x);
                this.scroll_offset.y += f32::from(delta.y);
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

        if let Some(bounds) = self.container_bounds {
            let extent = self.content_extent();
            let vw = f32::from(bounds.size.width);
            let vh = f32::from(bounds.size.height);

            canvas_div = canvas_div
                .child(Scrollbar::new(
                    gpui::Axis::Vertical,
                    vh,
                    extent.y,
                    -self.scroll_offset.y,
                ))
                .child(Scrollbar::new(
                    gpui::Axis::Horizontal,
                    vw,
                    extent.x,
                    -self.scroll_offset.x,
                ));
        }

        canvas_div
    }
}

impl PaneItem for DashboardPanel {
    fn tab_title(&self, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn serialization_key() -> &'static str {
        "dashboard"
    }

    fn serialize(&self, cx: &App) -> String {
        facet_json::to_string(&self.to_config(cx)).expect("dashboard panel config serializes")
    }
}

/// Persisted shape of [`DashboardPanel`].
#[derive(facet::Facet, Default)]
pub struct DashboardPanelConfig {
    pub title: String,
    pub next_id: u64,
    pub widgets: Vec<DashboardWidget>,
}

impl DashboardPanel {
    pub fn to_config(&self, _cx: &App) -> DashboardPanelConfig {
        DashboardPanelConfig {
            title: self.title.to_string(),
            next_id: self.next_id,
            widgets: self.widgets.clone(),
        }
    }
}

/// Rebuild a [`DashboardPanel`] from its persisted facet-json blob.
///
/// Returns a freshly defaulted dashboard if the blob fails to parse —
/// preferable to crashing on a stale or hand-edited config file.
pub fn deserialize_dashboard(db: Arc<DB>, blob: &str, cx: &mut App) -> Entity<DashboardPanel> {
    let cfg: DashboardPanelConfig = facet_json::from_str(blob).unwrap_or_default();

    let mut widget_views = HashMap::new();
    let mut widget_entities = HashMap::new();
    for widget in &cfg.widgets {
        let (view, widget_entity) = create_widget_view(&widget.kind, &widget.config, &db, cx);
        widget_views.insert(widget.id, view);
        widget_entities.insert(widget.id, widget_entity);
    }

    cx.new(|_cx| DashboardPanel {
        db,
        title: if cfg.title.is_empty() {
            SharedString::from("Dashboard")
        } else {
            SharedString::from(cfg.title)
        },
        widgets: cfg.widgets,
        widget_views,
        widget_entities,
        next_id: if cfg.next_id == 0 { 1 } else { cfg.next_id },
        editing: false,
        selected: None,
        container_bounds: None,
        scroll_offset: point(0.0, 0.0),
    })
}
