//! Free-form dashboard with pixel-positioned widgets.
//!
//! Submodules:
//! - [`widgets`] — per-kind factories and leaf renderers (image, monitor).
//! - [`connectors`] — schematic lines between widgets and free canvas points.
//! - [`interaction`] — drag and resize payloads plus the per-widget layout.
//! - [`chrome`] — grid overlay rendered above widgets in edit mode.
use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    AnyElement, AnyView, App, Bounds, Context, Entity, Hsla, IntoElement, Pixels, Point, Render,
    SharedString, Window, div, point, prelude::*, px,
};
use metor_db::DB;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::inspector::rows::{CommandRow, DefaultActionRow, InspectorRow, NavRow};
use crate::theme::theme;
use crate::views::{Scrollbar, TimeSeriesPlot};

use crate::tiles::PaneItem;

mod chrome;
pub mod connectors;
mod interaction;
mod widgets;

pub use connectors::{
    ArrowEnds, Connector, ConnectorAnchor, ConnectorBinding, ConnectorId, ConnectorStyle,
    LineShape, Side,
};

use widgets::create_widget_view;
pub use widgets::{WidgetRegistry, WidgetSpec};

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
    pub fn sequence_control() -> Self {
        Self(SharedString::new_static("sequence_control"))
    }
    pub fn attitude() -> Self {
        Self(SharedString::new_static("attitude"))
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

    /// Schematic lines over the canvas. Plain data: an anchor resolves
    /// against the live widget rects each frame, so nothing here needs
    /// updating when a widget moves.
    connectors: Vec<Connector>,
    /// Stream tasks for connectors whose colour is telemetry-bound, keyed
    /// like `widget_entities` so dropping an entry cancels its task.
    connector_live: HashMap<ConnectorId, connectors::ConnectorLive>,
    next_connector_id: u64,
    selected_connector: Option<ConnectorId>,
    tool: connectors::Tool,
    draft: connectors::Draft,

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
            connectors: Vec::new(),
            connector_live: HashMap::new(),
            next_connector_id: 1,
            selected_connector: None,
            tool: connectors::Tool::default(),
            draft: connectors::Draft::default(),
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

    fn alloc_connector_id(&mut self) -> ConnectorId {
        let id = ConnectorId(self.next_connector_id);
        self.next_connector_id += 1;
        id
    }

    pub fn add_connector(&mut self, connector: Connector, cx: &mut Context<Self>) {
        self.connectors.push(connector);
        cx.notify();
    }

    pub fn remove_connector(&mut self, id: ConnectorId, cx: &mut Context<Self>) {
        self.connectors.retain(|c| c.id != id);
        // Dropping the live entry cancels the binding task; leaving it would
        // keep streaming for the dashboard's whole lifetime.
        self.connector_live.remove(&id);
        if self.selected_connector == Some(id) {
            self.selected_connector = None;
        }
        cx.notify();
    }

    pub fn connectors(&self) -> &[Connector] {
        &self.connectors
    }

    /// Start (or drop) the stream tasks behind telemetry-bound connectors.
    fn ensure_connector_bindings(&mut self, cx: &mut Context<Self>) {
        let db = self.db.clone();
        // The connector list is cloned so `spawn` can borrow `cx` while
        // `reconcile_bindings` walks it.
        let list = self.connectors.clone();
        let thresholds: HashMap<ConnectorId, f64> = list
            .iter()
            .filter_map(|c| c.style.bind.as_ref().map(|b| (c.id, b.threshold)))
            .collect();
        let mut live = std::mem::take(&mut self.connector_live);
        connectors::reconcile_bindings(&list, &mut live, |id, component, element| {
            let threshold = thresholds.get(&id).copied().unwrap_or_default();
            connectors::spawn_binding(
                db.clone(),
                component,
                element,
                threshold,
                cx,
                move |this: &mut Self, on, cx| {
                    if let Some(state) = this.connector_live.get_mut(&id)
                        && state.on != on
                    {
                        state.on = on;
                        cx.notify();
                    }
                },
            )
        });
        self.connector_live = live;
    }

    /// Widgets paired with their inspectable entity and a palette label.
    ///
    /// Consumed by the palette's `Widget` category so each widget is
    /// reachable without opening the dashboard first.
    pub fn inspectable_widgets(&self, cx: &App) -> Vec<(WidgetId, gpui::AnyEntity, SharedString)> {
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

    fn add_widget(&mut self, kind: WidgetKind, config: String, cx: &mut Context<Self>) -> WidgetId {
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
        // Dropping the entity here is what cancels the widget's stream
        // tasks; a leftover entry keeps them alive for the dashboard's
        // whole lifetime.
        self.widget_entities.remove(&id);
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

    /// Enter connector-drawing mode. Requires edit mode, since the canvas
    /// only receives clicks there.
    pub fn start_connector(&mut self, cx: &mut Context<Self>) {
        self.editing = true;
        self.selected = None;
        self.selected_connector = None;
        self.tool = connectors::Tool::DrawConnector;
        self.draft = connectors::Draft::default();
        cx.notify();
    }

    /// Commit the draft, if it has enough anchors to be a line, and return
    /// to selection.
    fn finish_draft(&mut self, cx: &mut Context<Self>) {
        let points = std::mem::take(&mut self.draft.points);
        if points.len() >= 2 {
            let id = self.alloc_connector_id();
            self.connectors.push(Connector {
                id,
                points,
                style: ConnectorStyle::default(),
            });
            // Select it so the inspector opens on what was just drawn.
            self.selected_connector = Some(id);
        }
        self.draft = connectors::Draft::default();
        self.tool = connectors::Tool::Select;
        cx.notify();
    }

    fn cancel_draft(&mut self, cx: &mut Context<Self>) {
        self.draft = connectors::Draft::default();
        self.tool = connectors::Tool::Select;
        cx.notify();
    }

    /// The topmost widget under a canvas point. Later entries paint on top,
    /// so the search runs backwards.
    fn widget_at(&self, at: Point<f32>) -> Option<&DashboardWidget> {
        self.widgets.iter().rev().find(|w| {
            at.x >= w.rect.x
                && at.x <= w.rect.x + w.rect.w
                && at.y >= w.rect.y
                && at.y <= w.rect.y + w.rect.h
        })
    }

    /// Turn a click into an anchor: on a widget it snaps to the nearest
    /// edge, elsewhere it lands on the snap grid like a widget would.
    fn anchor_at(&self, at: Point<f32>) -> ConnectorAnchor {
        match self.widget_at(at) {
            Some(w) => {
                let (side, t) = connectors::nearest_side(&w.rect, (at.x, at.y));
                ConnectorAnchor::Widget { id: w.id, side, t }
            }
            None => ConnectorAnchor::Free {
                x: snap_px(at.x),
                y: snap_px(at.y),
            },
        }
    }

    /// The connector nearest a canvas point, within the grab radius.
    fn connector_at(&self, at: Point<f32>) -> Option<ConnectorId> {
        let pointer = point(px(at.x), px(at.y));
        self.connectors
            .iter()
            .filter_map(|c| {
                let points = connectors::resolve_all(c, &self.widgets)?;
                let d = crate::graph_canvas::distance_to_line(&points, c.style.shape, pointer);
                (d <= crate::graph_canvas::LINE_HIT_RADIUS).then_some((d, c.id))
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, id)| id)
    }

    fn handle_canvas_mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(at) = self.pixel_to_canvas(event.position) else {
            return;
        };
        match self.tool {
            connectors::Tool::DrawConnector => {
                // The second click of a double-click also lands an anchor,
                // which is what makes "click along, double-click to end" put
                // the final point where the user last clicked.
                if event.click_count >= 2 {
                    self.finish_draft(cx);
                    return;
                }
                let anchor = self.anchor_at(at);
                self.draft.points.push(anchor);
                cx.notify();
            }
            connectors::Tool::Select => {
                if !self.editing {
                    return;
                }
                // A click on a widget belongs to the widget; only empty
                // canvas reaches the connectors underneath.
                if self.widget_at(at).is_some() {
                    return;
                }
                let hit = self.connector_at(at);
                if hit != self.selected_connector {
                    self.selected_connector = hit;
                    cx.notify();
                }
            }
        }
    }

    /// Anchors of the in-progress draft, plus the cursor, resolved for the
    /// preview stroke.
    fn draft_preview(&self) -> Option<SmallVec<[Point<Pixels>; 6]>> {
        if self.tool != connectors::Tool::DrawConnector || self.draft.points.is_empty() {
            return None;
        }
        let mut points: SmallVec<[Point<Pixels>; 6]> = self
            .draft
            .points
            .iter()
            .filter_map(|a| connectors::resolve(a, &self.widgets).map(|(x, y)| point(px(x), px(y))))
            .collect();
        if let Some((x, y)) = self.draft.cursor {
            points.push(point(px(x), px(y)));
        }
        (points.len() >= 2).then_some(points)
    }

    /// Paint the connectors on one side of the widget stack.
    ///
    /// Two passes rather than one: a schematic pipe belongs *under* the box
    /// it enters, while a callout leader from a diagram to a live readout has
    /// to cross over it. Splitting on `on_top` gets both from one model.
    fn render_connector_layer(&self, on_top: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let selection = theme.selection_bg;
        let offset = self.scroll_offset;

        struct Painted {
            points: SmallVec<[Point<Pixels>; 6]>,
            style: crate::graph_canvas::LineStyle,
            color: Hsla,
            arrow: ArrowEnds,
            selected: bool,
        }

        let painted: Vec<Painted> = self
            .connectors
            .iter()
            .filter(|c| c.style.on_top == on_top)
            .filter_map(|c| {
                Some(Painted {
                    points: connectors::resolve_all(c, &self.widgets)?,
                    style: c.stroke(),
                    color: connectors::line_color(c, self.connector_live.get(&c.id), &theme),
                    arrow: c.style.arrow,
                    selected: self.selected_connector == Some(c.id),
                })
            })
            .collect();

        // The draft rides the top layer so it stays visible over widgets
        // while its endpoints are still being chosen.
        let draft = on_top.then(|| self.draft_preview()).flatten();
        let draft_color = theme.text_tertiary;

        gpui::canvas(
            move |bounds, _window, _cx| bounds,
            move |_, bounds: Bounds<Pixels>, window, _cx| {
                let origin = point(
                    bounds.origin.x + px(offset.x),
                    bounds.origin.y + px(offset.y),
                );
                if let Some(points) = &draft {
                    crate::graph_canvas::paint_line(
                        origin,
                        points,
                        crate::graph_canvas::LineStyle {
                            width: px(1.5),
                            dashed: true,
                            shape: LineShape::default(),
                        },
                        draft_color,
                        window,
                    );
                }
                for line in &painted {
                    if line.selected {
                        // A wider pass underneath reads as a halo without
                        // needing a second colour for every theme.
                        let mut halo = line.style;
                        halo.width = line.style.width + px(4.0);
                        crate::graph_canvas::paint_line(
                            origin,
                            &line.points,
                            halo,
                            selection,
                            window,
                        );
                    }
                    crate::graph_canvas::paint_line(
                        origin,
                        &line.points,
                        line.style,
                        line.color,
                        window,
                    );

                    if line.arrow == ArrowEnds::None {
                        continue;
                    }
                    // Arrowheads follow the drawn run, not the raw anchors,
                    // so an elbow or a curve gets the right approach angle.
                    let drawn = crate::graph_canvas::drawn_polyline(&line.points, line.style.shape);
                    let shift = |p: Point<Pixels>| point(origin.x + p.x, origin.y + p.y);
                    if drawn.len() >= 2 {
                        let n = drawn.len();
                        crate::graph_canvas::paint_arrowhead(
                            shift(drawn[n - 1]),
                            shift(drawn[n - 2]),
                            line.color,
                            window,
                        );
                        if line.arrow == ArrowEnds::Both {
                            crate::graph_canvas::paint_arrowhead(
                                shift(drawn[0]),
                                shift(drawn[1]),
                                line.color,
                                window,
                            );
                        }
                    }
                }
            },
        )
        .size_full()
        .absolute()
    }

    /// Connector labels, as positioned text rather than painted glyphs.
    fn render_connector_labels(&self, on_top: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let theme = theme(cx);
        self.connectors
            .iter()
            .filter(|c| c.style.on_top == on_top && !c.style.label.is_empty())
            .filter_map(|c| {
                let points = connectors::resolve_all(c, &self.widgets)?;
                let at = connectors::label_anchor(&points);
                Some(
                    div()
                        .absolute()
                        .left(at.x + px(self.scroll_offset.x + 4.0))
                        .top(at.y + px(self.scroll_offset.y - 14.0))
                        .px(px(3.0))
                        .rounded(px(3.0))
                        .bg(theme.pill_bg)
                        .text_size(px(10.0))
                        .text_color(theme.text_secondary)
                        .child(SharedString::from(c.style.label.clone()))
                        .into_any_element(),
                )
            })
            .collect()
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

    rows.push(Box::new(CommandRow::new("Draw Connector", {
        let dashboard = dashboard.clone();
        Arc::new(move |_window, cx| {
            dashboard.update(cx, |this, cx| this.start_connector(cx));
        })
    })));

    let connector_list: Vec<(ConnectorId, SharedString)> = dashboard
        .read(cx)
        .connectors
        .iter()
        .map(|c| (c.id, connector_label(c)))
        .collect();
    if !connector_list.is_empty() {
        rows.push(Box::new(NavRow::new(
            "Connectors",
            SharedString::from(format!("{} lines", connector_list.len())),
            {
                let dashboard = dashboard.clone();
                Box::new(move |_cx| {
                    connector_list
                        .iter()
                        .map(|(id, label)| {
                            let id = *id;
                            let dashboard = dashboard.clone();
                            Box::new(NavRow::new(
                                label.clone(),
                                SharedString::new_static(""),
                                Box::new(move |_cx| connector_rows(dashboard.clone(), id)),
                            )) as Box<dyn InspectorRow>
                        })
                        .collect()
                })
            },
        )));
    }

    rows.push(Box::new(NavRow::new(
        "Rename",
        SharedString::new_static(""),
        {
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
        },
    )));

    rows
}

fn add_widget_rows(dashboard: Entity<DashboardPanel>, db: Arc<DB>) -> Vec<Box<dyn InspectorRow>> {
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
    rows.push(Box::new(NavRow::new(
        "Component Text",
        SharedString::new_static(""),
        {
            let dashboard = dashboard.clone();
            let db = db.clone();
            Box::new(move |_cx| {
                component_picker_rows(dashboard.clone(), db.clone(), WidgetKind::text())
            })
        },
    )));
    rows.push(Box::new(CommandRow::new("Component Table", {
        let dashboard = dashboard.clone();
        Arc::new(move |_window, cx| {
            dashboard.update(cx, |this, cx| {
                this.add_widget(WidgetKind::table(), "{}".to_string(), cx);
            });
        })
    })));
    rows.push(Box::new(NavRow::new(
        "Monitor",
        SharedString::new_static(""),
        {
            let dashboard = dashboard.clone();
            let db = db.clone();
            Box::new(move |_cx| {
                component_picker_rows(dashboard.clone(), db.clone(), WidgetKind::monitor())
            })
        },
    )));
    rows.push(Box::new(NavRow::new(
        "Traffic Light",
        SharedString::new_static(""),
        {
            let dashboard = dashboard.clone();
            let db = db.clone();
            Box::new(move |_cx| {
                component_picker_rows(dashboard.clone(), db.clone(), WidgetKind::traffic_light())
            })
        },
    )));
    rows.push(Box::new(NavRow::new(
        "Traffic Light Grid",
        SharedString::new_static(""),
        {
            let dashboard = dashboard.clone();
            Box::new(move |_cx| traffic_light_grid_pattern_rows(dashboard.clone()))
        },
    )));
    rows.push(Box::new(NavRow::new(
        "Meter",
        SharedString::new_static(""),
        {
            let dashboard = dashboard.clone();
            let db = db.clone();
            Box::new(move |_cx| {
                instrument_widget_rows(dashboard.clone(), db.clone(), WidgetKind::meter(), |seed| {
                    serde_json::to_string(&crate::views::MeterConfig::from(seed))
                        .expect("meter config serializes")
                })
            })
        },
    )));
    rows.push(Box::new(NavRow::new(
        "Gauge",
        SharedString::new_static(""),
        {
            let dashboard = dashboard.clone();
            let db = db.clone();
            Box::new(move |_cx| {
                instrument_widget_rows(dashboard.clone(), db.clone(), WidgetKind::gauge(), |seed| {
                    serde_json::to_string(&crate::views::GaugeConfig::from(seed))
                        .expect("gauge config serializes")
                })
            })
        },
    )));
    rows.push(Box::new(NavRow::new(
        "State Chip",
        SharedString::new_static(""),
        {
            let dashboard = dashboard.clone();
            let db = db.clone();
            Box::new(move |_cx| {
                instrument_widget_rows(
                    dashboard.clone(),
                    db.clone(),
                    WidgetKind::state_chip(),
                    |seed| {
                        let cfg = crate::views::StateChipConfig {
                            component: seed.component,
                            element: seed.element,
                            label: Some(seed.label),
                            ..Default::default()
                        };
                        serde_json::to_string(&cfg).expect("state chip config serializes")
                    },
                )
            })
        },
    )));
    rows.push(Box::new(NavRow::new(
        "Attitude",
        SharedString::new_static(""),
        {
            let dashboard = dashboard.clone();
            let db = db.clone();
            Box::new(move |_cx| {
                let dashboard = dashboard.clone();
                crate::tiles::panels::component_picker_rows(
                    db.clone(),
                    move |_component_id, name, cx| {
                        let cfg = crate::views::AttitudeConfig {
                            component: name.clone(),
                            ..Default::default()
                        };
                        let blob = serde_json::to_string(&cfg).expect("attitude config serializes");
                        dashboard.update(cx, |this, cx| {
                            this.add_widget(WidgetKind::attitude(), blob, cx);
                        });
                    },
                )
            })
        },
    )));
    rows.push(Box::new(NavRow::new(
        "Sequence Control",
        SharedString::new_static(""),
        {
            let dashboard = dashboard.clone();
            Box::new(move |cx| {
                let dashboard = dashboard.clone();
                crate::views::sequence_control::channel_picker_rows(cx, move |channel, cx| {
                    let cfg = crate::views::SequenceControlConfig {
                        channel,
                        compact: false,
                    };
                    let blob =
                        serde_json::to_string(&cfg).expect("sequence control config serializes");
                    dashboard.update(cx, |this, cx| {
                        this.add_widget(WidgetKind::sequence_control(), blob, cx);
                    });
                })
            })
        },
    )));
    rows.push(Box::new(NavRow::new(
        "Image",
        SharedString::new_static(""),
        {
            let dashboard = dashboard.clone();
            Box::new(move |_cx| image_path_rows(dashboard.clone()))
        },
    )));
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
                    // Pick the kind-specific config struct so any future
                    // divergence between the two doesn't silently lose the
                    // user's component selection.
                    let component = name_clone.clone();
                    let config = if kind == WidgetKind::monitor() {
                        let cfg = widgets::MonitorWidgetConfig {
                            component,
                            ..Default::default()
                        };
                        serde_json::to_string(&cfg)
                    } else if kind == WidgetKind::traffic_light() {
                        let cfg = widgets::TrafficLightWidgetConfig {
                            component,
                            color: None,
                        };
                        serde_json::to_string(&cfg)
                    } else {
                        let cfg = widgets::TextWidgetConfig { component };
                        serde_json::to_string(&cfg)
                    }
                    .expect("component widget config serializes");
                    let kind = kind.clone();
                    dashboard.update(cx, |this, cx| {
                        this.add_widget(kind, config, cx);
                    });
                }),
            )) as Box<dyn InspectorRow>
        })
        .collect()
}

/// Trace wizard for the scalar instruments: each picked element becomes its
/// own widget of `kind`, with `blob` turning the seed into that kind's
/// config. Mirrors the pane-side `instrument_wizard_rows`.
fn instrument_widget_rows(
    dashboard: Entity<DashboardPanel>,
    db: Arc<DB>,
    kind: WidgetKind,
    blob: fn(crate::tiles::panels::ScaleSeed) -> String,
) -> Vec<Box<dyn InspectorRow>> {
    let db_outer = db.clone();
    crate::inspector::trace_picker::select_traces_wizard_rows(
        db,
        Arc::new(|_cx| 0),
        Arc::new(move |traces, _window, cx| {
            let seeds = crate::tiles::panels::scale_seeds_for_traces(&db_outer, &traces, cx);
            dashboard.update(cx, |this, cx| {
                for seed in seeds {
                    this.add_widget(kind.clone(), blob(seed), cx);
                }
            });
        }),
    )
}

/// Single-question wizard for "+ widget → Traffic Light Grid": prompts for
/// a glob pattern, then adds a `traffic_light_grid` widget with that
/// pattern. Mirrors [`image_path_rows`].
fn traffic_light_grid_pattern_rows(
    dashboard: Entity<DashboardPanel>,
) -> Vec<Box<dyn InspectorRow>> {
    vec![crate::views::traffic_light_grid::glob_prompt_row(Arc::new(
        move |input, _window, cx| {
            let cfg = widgets::TrafficLightGridWidgetConfig {
                pattern: input.to_string(),
                color: None,
            };
            let config =
                serde_json::to_string(&cfg).expect("traffic light grid widget config serializes");
            dashboard.update(cx, |this, cx| {
                this.add_widget(WidgetKind::traffic_light_grid(), config, cx);
            });
        },
    ))]
}

fn image_path_rows(dashboard: Entity<DashboardPanel>) -> Vec<Box<dyn InspectorRow>> {
    vec![Box::new(DefaultActionRow {
        label: "Image file path...".into(),
        callback: Arc::new(move |input, _window, cx| {
            if !input.is_empty() {
                let cfg = widgets::ImageWidgetConfig {
                    path: input.to_string(),
                    data: String::new(),
                };
                let config = serde_json::to_string(&cfg).expect("image widget config serializes");
                dashboard.update(cx, |this, cx| {
                    this.add_widget(WidgetKind::image(), config, cx);
                });
            }
        }),
    })]
}

/// A connector's palette label: its own label if it has one, else its shape
/// and endpoint count, which is enough to tell two lines apart.
fn connector_label(c: &Connector) -> SharedString {
    if !c.style.label.is_empty() {
        return SharedString::from(c.style.label.clone());
    }
    SharedString::from(format!(
        "{:?} #{} ({} pts)",
        c.style.shape,
        c.id.0,
        c.points.len()
    ))
}

/// Property rows for one connector: appearance, then the destructive action
/// last.
fn connector_rows(
    dashboard: Entity<DashboardPanel>,
    id: ConnectorId,
) -> Vec<Box<dyn InspectorRow>> {
    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();

    /// Mutate one connector in place and repaint.
    fn edit(
        dashboard: &Entity<DashboardPanel>,
        id: ConnectorId,
        cx: &mut App,
        f: impl FnOnce(&mut Connector),
    ) {
        dashboard.update(cx, |this, cx| {
            if let Some(c) = this.connectors.iter_mut().find(|c| c.id == id) {
                f(c);
                cx.notify();
            }
        });
    }

    for (label, shape) in [
        ("Shape: Orthogonal", LineShape::Orthogonal),
        ("Shape: Straight", LineShape::Straight),
        ("Shape: Curved", LineShape::Curved),
    ] {
        let dashboard = dashboard.clone();
        rows.push(Box::new(CommandRow::new(
            label,
            Arc::new(move |_window, cx| {
                edit(&dashboard, id, cx, |c| c.style.shape = shape);
            }),
        )));
    }

    rows.push(Box::new(CommandRow::new("Toggle Dashed", {
        let dashboard = dashboard.clone();
        Arc::new(move |_window, cx| {
            edit(&dashboard, id, cx, |c| c.style.dashed = !c.style.dashed);
        })
    })));

    rows.push(Box::new(CommandRow::new("Toggle Arrowhead", {
        let dashboard = dashboard.clone();
        Arc::new(move |_window, cx| {
            edit(&dashboard, id, cx, |c| {
                c.style.arrow = match c.style.arrow {
                    ArrowEnds::None => ArrowEnds::End,
                    ArrowEnds::End => ArrowEnds::Both,
                    ArrowEnds::Both => ArrowEnds::None,
                };
            });
        })
    })));

    rows.push(Box::new(CommandRow::new("Toggle Draw Over Widgets", {
        let dashboard = dashboard.clone();
        Arc::new(move |_window, cx| {
            edit(&dashboard, id, cx, |c| c.style.on_top = !c.style.on_top);
        })
    })));

    rows.push(Box::new(NavRow::new(
        "Label",
        SharedString::new_static(""),
        {
            let dashboard = dashboard.clone();
            Box::new(move |_cx| {
                let dashboard = dashboard.clone();
                vec![Box::new(DefaultActionRow {
                    label: "Connector label...".into(),
                    callback: Arc::new(move |input, _window, cx| {
                        let text = input.to_string();
                        edit(&dashboard, id, cx, |c| c.style.label = text);
                    }),
                }) as Box<dyn InspectorRow>]
            })
        },
    )));

    rows.push(Box::new(CommandRow::new("Delete", {
        let dashboard = dashboard.clone();
        Arc::new(move |_window, cx| {
            dashboard.update(cx, |this, cx| this.remove_connector(id, cx));
        })
    })));

    rows
}

/// The floating strip the modal canvas tools live in, top-centre so it
/// clears the edit-mode badge in the corner.
fn canvas_toolbar(theme: &crate::theme::Theme) -> gpui::Div {
    div()
        .absolute()
        .top(px(4.0))
        .left(px(8.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .px(px(8.0))
        .py(px(3.0))
        .bg(theme.pill_bg)
        .border_1()
        .border_color(theme.pill_border)
        .rounded(px(4.0))
        .text_size(px(11.0))
}

fn widget_display_label(widget: &DashboardWidget, cx: &App) -> SharedString {
    (widgets::widget_spec(&widget.kind, cx).label)(widget)
}

impl Render for DashboardPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_views(cx);
        self.ensure_connector_bindings(cx);
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

        // Under-layer: schematic runs that should disappear into the boxes
        // they enter.
        canvas_div = canvas_div.child(self.render_connector_layer(false, cx));
        for label in self.render_connector_labels(false, cx) {
            canvas_div = canvas_div.child(label);
        }

        // Vec order is z-order: later widgets paint on top.
        let widgets: Vec<DashboardWidget> = self.widgets.clone();
        for widget in &widgets {
            canvas_div = canvas_div.child(self.render_widget(widget, cx));
        }

        // Over-layer: callout leaders, which have to cross the widgets they
        // point at.
        canvas_div = canvas_div.child(self.render_connector_layer(true, cx));
        for label in self.render_connector_labels(true, cx) {
            canvas_div = canvas_div.child(label);
        }

        canvas_div = canvas_div
            .on_drag_move(cx.listener(Self::handle_widget_drag_move))
            .on_drag_move(cx.listener(Self::handle_widget_resize_move))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(Self::handle_canvas_mouse_down),
            );

        if self.tool == connectors::Tool::DrawConnector {
            canvas_div = canvas_div
                .cursor(gpui::CursorStyle::Crosshair)
                .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                    if let Some(at) = this.pixel_to_canvas(event.position) {
                        this.draft.cursor = Some((at.x, at.y));
                        cx.notify();
                    }
                }));
        }

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

        // Drawing and selection are modal, so they get on-canvas controls
        // rather than living only behind a keybinding.
        if self.tool == connectors::Tool::DrawConnector {
            let placed = self.draft.points.len();
            canvas_div = canvas_div.child(
                canvas_toolbar(&theme)
                    .child(
                        div()
                            .text_color(theme.text_secondary)
                            .child(SharedString::from(if placed < 2 {
                                "Click to place points".to_string()
                            } else {
                                format!("{placed} points — double-click to finish")
                            })),
                    )
                    .child(
                        crate::views::sequence_panel::pill_button(
                            &theme,
                            ("conn-finish", 0),
                            "Finish",
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.finish_draft(cx))),
                    )
                    .child(
                        crate::views::sequence_panel::pill_button(
                            &theme,
                            ("conn-cancel", 0),
                            "Cancel",
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_draft(cx))),
                    ),
            );
        } else if let Some(selected) = self.selected_connector {
            canvas_div = canvas_div.child(
                canvas_toolbar(&theme)
                    .child(div().text_color(theme.text_secondary).child("Connector"))
                    .child(
                        crate::views::sequence_panel::pill_button(
                            &theme,
                            ("conn-delete", 0),
                            "Delete",
                        )
                        .text_color(theme.error_accent)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.remove_connector(selected, cx);
                        })),
                    )
                    .child(
                        crate::views::sequence_panel::pill_button(
                            &theme,
                            ("conn-deselect", 0),
                            "Deselect",
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.selected_connector = None;
                            cx.notify();
                        })),
                    ),
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

/// Persisted shape of [`DashboardPanel`].
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DashboardPanelConfig {
    pub title: String,
    pub next_id: u64,
    pub widgets: Vec<DashboardWidget>,
    pub connectors: Vec<Connector>,
    pub next_connector_id: u64,
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
                if let Some(entity) = self.widget_entities.get(&w.id)
                    && let Some(blob) =
                        widgets::serialize_widget_state(&w.kind, entity, &w.config, cx)
                {
                    w.config = blob;
                }
                w
            })
            .collect();
        DashboardPanelConfig {
            title: self.title.to_string(),
            next_id: self.next_id,
            widgets,
            connectors: self.connectors.clone(),
            next_connector_id: self.next_connector_id,
        }
    }
}

/// Where connector id allocation resumes for a loaded document.
///
/// A preset authored by hand or by the Python builder carries connectors but
/// no counter, so the counter is seeded past the highest id actually present
/// rather than trusted — otherwise the first line drawn on a shipped preset
/// collides with one already in it.
fn seed_connector_id(connectors: &[Connector], saved: u64) -> u64 {
    connectors
        .iter()
        .map(|c| c.id.0 + 1)
        .chain(std::iter::once(saved))
        .max()
        .unwrap_or(1)
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free(x: f32, y: f32) -> ConnectorAnchor {
        ConnectorAnchor::Free { x, y }
    }

    #[test]
    fn connectors_round_trip_with_the_dashboard() {
        let cfg = DashboardPanelConfig {
            title: "P&ID".into(),
            next_id: 4,
            widgets: Vec::new(),
            connectors: vec![Connector {
                id: ConnectorId(2),
                points: vec![free(0.0, 0.0), free(50.0, 90.0)],
                style: ConnectorStyle {
                    label: "feed".into(),
                    on_top: true,
                    ..Default::default()
                },
            }],
            next_connector_id: 3,
        };
        let blob = serde_json::to_string(&cfg).unwrap();
        let back: DashboardPanelConfig = serde_json::from_str(&blob).unwrap();
        assert_eq!(back.connectors.len(), 1);
        assert_eq!(back.connectors[0].style.label, "feed");
        assert!(back.connectors[0].style.on_top);
        assert_eq!(back.next_connector_id, 3);
    }

    /// Cross-language pin: a dashboard emitted by the Python preset builder
    /// (`metor_config.Dashboard`) must parse into this config exactly.
    ///
    /// The widget `config` blob is an opaque string to the FSW-side preset
    /// validator, so nothing upstream catches a shape mismatch — without this
    /// test, a rename here turns a shipped dashboard into placeholder tiles
    /// with no error anywhere.
    #[test]
    fn a_python_authored_dashboard_parses() {
        let blob = r##"{"title": "ADCS", "next_id": 5, "widgets": [{"id": 1, "rect": {"x": 20.0, "y": 20.0, "w": 90.0, "h": 200.0}, "kind": "meter", "config": "{\"component\": \"sat1.plant.wheels.h\", \"element\": 0, \"min\": -0.04, \"max\": 0.04, \"unit\": \"N m s\", \"orientation\": \"Vertical\"}"}, {"id": 2, "rect": {"x": 200.0, "y": 20.0, "w": 150.0, "h": 60.0}, "kind": "state_chip", "config": "{\"component\": \"sat1.mode.mode_cmd\", \"element\": 0, \"states\": [{\"value\": 0.0, \"label\": \"IDLE\"}, {\"value\": 2.0, \"label\": \"POINTING\", \"color\": \"#a6e3a1ff\"}], \"unknown_label\": \"\"}"}, {"id": 3, "rect": {"x": 20.0, "y": 260.0, "w": 220.0, "h": 260.0}, "kind": "attitude", "config": "{\"component\": \"sat1.nav.attitude_estimate.q_hat_b_eci\", \"element_offset\": 0, \"vectors\": [{\"component\": \"sat1.plant.sensors.mag_b\", \"label\": \"mag\"}]}"}, {"id": 4, "rect": {"x": 200.0, "y": 120.0, "w": 260.0, "h": 110.0}, "kind": "sequence_control", "config": "{\"channel\": \"mode\", \"compact\": false}"}], "connectors": [{"id": 1, "points": [{"Widget": {"id": 3, "side": "Top", "t": 0.2045}}, {"Widget": {"id": 1, "side": "Bottom", "t": 1.0}}], "style": {"width": 1.5, "dashed": false, "shape": "Orthogonal", "arrow": "End", "label": "momentum", "on_top": false, "bind": {"component": "sat1.plant.wheels.arm", "element": 0, "threshold": 0.0}}}, {"id": 2, "points": [{"Widget": {"id": 2, "side": "Right", "t": 0.5}}, {"Free": {"x": 600.0, "y": 60.0}}], "style": {"width": 1.5, "dashed": true, "shape": "Curved", "arrow": "None", "label": "", "on_top": true}}], "next_connector_id": 3}"##;

        let cfg: DashboardPanelConfig = serde_json::from_str(blob).unwrap();
        assert_eq!(cfg.title, "ADCS");
        assert_eq!(
            cfg.widgets
                .iter()
                .map(|w| w.kind.0.as_ref())
                .collect::<Vec<_>>(),
            ["meter", "state_chip", "attitude", "sequence_control"]
        );

        // Every widget kind must be one the registry knows; an unregistered
        // kind renders as "? unknown kind" rather than failing to parse.
        for w in &cfg.widgets {
            assert!(
                w.kind == WidgetKind::meter()
                    || w.kind == WidgetKind::state_chip()
                    || w.kind == WidgetKind::attitude()
                    || w.kind == WidgetKind::sequence_control(),
                "unregistered kind {:?}",
                w.kind
            );
        }

        // Each widget's opaque blob must parse into that kind's config.
        let meter: crate::views::MeterConfig =
            serde_json::from_str(&cfg.widgets[0].config).unwrap();
        assert_eq!(meter.component, "sat1.plant.wheels.h");
        assert_eq!((meter.min, meter.max), (-0.04, 0.04));
        assert!(matches!(
            meter.orientation,
            crate::views::Orientation::Vertical
        ));

        let chip: crate::views::StateChipConfig =
            serde_json::from_str(&cfg.widgets[1].config).unwrap();
        assert_eq!(chip.states.len(), 2);
        assert_eq!(chip.states[1].label, "POINTING");
        assert!(chip.states[1].color.is_some());

        let att: crate::views::AttitudeConfig =
            serde_json::from_str(&cfg.widgets[2].config).unwrap();
        assert_eq!(att.vectors[0].label, "mag");

        let seq: crate::views::SequenceControlConfig =
            serde_json::from_str(&cfg.widgets[3].config).unwrap();
        assert_eq!(seq.channel, "mode");

        assert_eq!(cfg.connectors.len(), 2);
        let schematic = &cfg.connectors[0];
        assert_eq!(schematic.style.shape, LineShape::Orthogonal);
        assert_eq!(schematic.style.arrow, ArrowEnds::End);
        assert!(!schematic.style.on_top);
        assert_eq!(
            schematic.style.bind.as_ref().unwrap().component,
            "sat1.plant.wheels.arm"
        );
        assert!(matches!(
            schematic.points[0],
            ConnectorAnchor::Widget {
                id: WidgetId(3),
                side: Side::Top,
                ..
            }
        ));

        let leader = &cfg.connectors[1];
        assert_eq!(leader.style.shape, LineShape::Curved);
        assert!(leader.style.on_top && leader.style.dashed);
        assert!(matches!(leader.points[1], ConnectorAnchor::Free { .. }));

        // Both connectors resolve once their widgets are in place.
        for c in &cfg.connectors {
            assert!(
                connectors::resolve_all(c, &cfg.widgets).is_some(),
                "connector {:?} did not resolve",
                c.id
            );
        }
    }

    #[test]
    fn a_dashboard_saved_before_connectors_existed_still_loads() {
        let legacy = r#"{"title":"Old","next_id":3,"widgets":[]}"#;
        let cfg: DashboardPanelConfig = serde_json::from_str(legacy).unwrap();
        assert_eq!(cfg.title, "Old");
        assert!(cfg.connectors.is_empty());
        assert_eq!(cfg.next_connector_id, 0);
    }

    fn line(id: u64) -> Connector {
        Connector {
            id: ConnectorId(id),
            points: vec![free(0.0, 0.0), free(1.0, 1.0)],
            style: ConnectorStyle::default(),
        }
    }

    /// A hand-authored preset carries connector ids but no counter, so the
    /// next drawn line must not collide with one already in the document.
    #[test]
    fn the_id_counter_is_seeded_past_authored_connectors() {
        assert_eq!(seed_connector_id(&[line(7), line(3)], 0), 8);
    }

    #[test]
    fn a_saved_counter_wins_when_it_is_ahead() {
        assert_eq!(seed_connector_id(&[line(2)], 40), 40);
    }

    #[test]
    fn an_empty_dashboard_starts_at_one() {
        assert_eq!(seed_connector_id(&[], 0), 1);
    }
}

/// Rebuild a [`DashboardPanel`] from its persisted JSON blob.
///
/// Returns a freshly defaulted dashboard if the blob fails to parse —
/// preferable to crashing on a stale or hand-edited config file.
pub fn deserialize_dashboard(db: Arc<DB>, blob: &str, cx: &mut App) -> Entity<DashboardPanel> {
    let cfg: DashboardPanelConfig = serde_json::from_str(blob).unwrap_or_default();

    let mut widget_views = HashMap::new();
    let mut widget_entities = HashMap::new();
    for widget in &cfg.widgets {
        let (view, widget_entity) = create_widget_view(&widget.kind, &widget.config, &db, cx);
        widget_views.insert(widget.id, view);
        widget_entities.insert(widget.id, widget_entity);
    }

    let next_connector_id = seed_connector_id(&cfg.connectors, cfg.next_connector_id);

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
        connectors: cfg.connectors,
        connector_live: HashMap::new(),
        next_connector_id,
        selected_connector: None,
        tool: connectors::Tool::default(),
        draft: connectors::Draft::default(),
        next_id: if cfg.next_id == 0 { 1 } else { cfg.next_id },
        editing: false,
        selected: None,
        container_bounds: None,
        scroll_offset: point(0.0, 0.0),
    })
}
