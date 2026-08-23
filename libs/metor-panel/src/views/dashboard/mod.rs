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
    AnyElement, App, Bounds, Context, Entity, Hsla, IntoElement, Pixels, Point, Render,
    SharedString, Window, div, point, prelude::*, px,
};
use metor_db::DB;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::inspector::rows::{CommandRow, DefaultActionRow, InspectorRow, NavRow};
use crate::theme::theme;
use crate::views::Scrollbar;

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
pub use widgets::{
    TileAddFlow, TileViewSpec, WidgetAddFlow, WidgetLive, WidgetRegistry, WidgetSpec,
};

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
/// Live widget handles are kept outside the persisted document.
pub struct DashboardPanel {
    db: Arc<DB>,
    title: SharedString,
    widgets: Vec<DashboardWidget>,
    widget_live: HashMap<WidgetId, WidgetLive>,

    /// Schematic lines over the canvas. Plain data: an anchor resolves
    /// against the live widget rects each frame, so nothing here needs
    /// updating when a widget moves.
    connectors: Vec<Connector>,
    /// Stream tasks for connectors whose colour is telemetry-bound, keyed
    /// like `widget_live` so dropping an entry cancels its task.
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
            widget_live: HashMap::new(),
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

    fn open_widget_inspector(
        &self,
        widget_id: WidgetId,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(live) = self.widget_live.get(&widget_id) {
            window.dispatch_action(
                Box::new(crate::inspector::InspectEntity {
                    entity: live.inspect.clone(),
                    position,
                }),
                cx,
            );
        }
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
            Box::new(move |cx| add_widget_rows(dashboard.clone(), db.clone(), cx))
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

fn add_widget_rows(
    dashboard: Entity<DashboardPanel>,
    db: Arc<DB>,
    cx: &App,
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
                crate::inspector::trace_picker::select_traces_wizard_rows(
                    db.clone(),
                    Arc::new(|_cx| 0),
                    Arc::new(move |traces, _window, cx| {
                        let config =
                            serde_json::to_string(&crate::views::time_series::PlotPanelConfig {
                                traces: traces
                                    .iter()
                                    .map(crate::views::time_series::TraceConfig::from)
                                    .collect(),
                                ..Default::default()
                            })
                            .expect("plot config serializes");
                        dashboard.update(cx, |this, cx| {
                            this.add_widget(WidgetKind::plot(), config, cx);
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
        "Annunciator",
        SharedString::new_static(""),
        {
            let dashboard = dashboard.clone();
            Box::new(move |_cx| annunciator_pattern_rows(dashboard.clone()))
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
                crate::inspector::trace_picker::component_picker_rows(
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

    for (label, add_flow) in cx.global::<WidgetRegistry>().add_flows() {
        rows.push(Box::new(NavRow::new(label, "", {
            let dashboard = dashboard.clone();
            let db = db.clone();
            Box::new(move |cx| add_flow(dashboard.clone(), db.clone(), cx))
        })));
    }

    rows
}

/// The component list for a widget kind, preceded by the expression row so
/// typing `=` binds the widget to a computed channel instead of a named one.
///
/// Both paths end in the same place: what the widget's config stores is a
/// string, and a leading `=` is what tells the builder to compile it rather
/// than look it up.
fn component_picker_rows(
    dashboard: Entity<DashboardPanel>,
    db: Arc<DB>,
    kind: WidgetKind,
) -> Vec<Box<dyn InspectorRow>> {
    let mut rows: Vec<Box<dyn InspectorRow>> = Vec::new();
    if kind == WidgetKind::monitor() {
        let dashboard = dashboard.clone();
        rows.push(Box::new(crate::inspector::rows::ExpressionRow::new(
            db.clone(),
            Arc::new(move |_id, text, cx| {
                let config = serde_json::to_string(&widgets::MonitorWidgetConfig {
                    component: text,
                    ..Default::default()
                })
                .expect("monitor config serializes");
                dashboard.update(cx, |this, cx| {
                    this.add_widget(WidgetKind::monitor(), config, cx);
                });
            }),
        )));
    }
    rows.extend(crate::inspector::trace_picker::list_components(&db)
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
        }));
    rows
}

/// Trace wizard for the scalar instruments: each picked element becomes its
/// own widget of `kind`, with `blob` turning the seed into that kind's
/// config. Mirrors the pane-side `instrument_wizard_rows`.
fn instrument_widget_rows(
    dashboard: Entity<DashboardPanel>,
    db: Arc<DB>,
    kind: WidgetKind,
    blob: fn(crate::views::instrument::ScaleSeed) -> String,
) -> Vec<Box<dyn InspectorRow>> {
    let db_outer = db.clone();
    crate::inspector::trace_picker::select_traces_wizard_rows(
        db,
        Arc::new(|_cx| 0),
        Arc::new(move |traces, _window, cx| {
            let seeds = crate::views::instrument::scale_seeds_for_traces(&db_outer, &traces, cx);
            dashboard.update(cx, |this, cx| {
                for seed in seeds {
                    this.add_widget(kind.clone(), blob(seed), cx);
                }
            });
        }),
    )
}

/// Single-question wizard for "+ widget → Annunciator": prompts for a glob
/// pattern, then adds an `annunciator` widget with that pattern. Mirrors
/// [`image_path_rows`].
fn annunciator_pattern_rows(dashboard: Entity<DashboardPanel>) -> Vec<Box<dyn InspectorRow>> {
    vec![crate::views::annunciator::glob_prompt_row(Arc::new(
        move |input, _window, cx| {
            let cfg = crate::views::annunciator::seeded_config(&input);
            let config = serde_json::to_string(&cfg).expect("annunciator widget config serializes");
            dashboard.update(cx, |this, cx| {
                this.add_widget(WidgetKind::annunciator(), config, cx);
            });
        },
    ))]
}

fn image_path_rows(dashboard: Entity<DashboardPanel>) -> Vec<Box<dyn InspectorRow>> {
    vec![Box::new(DefaultActionRow::new(
        "Image file path...",
        Arc::new(move |input, _window, cx| {
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
    ))]
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
                vec![Box::new(DefaultActionRow::new(
                    "Connector label...",
                    Arc::new(move |input, _window, cx| {
                        let text = input.to_string();
                        edit(&dashboard, id, cx, |c| c.style.label = text);
                    }),
                )) as Box<dyn InspectorRow>]
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
    pub widgets: Vec<DashboardWidget>,
    pub connectors: Vec<Connector>,
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
            connectors: self.connectors.clone(),
        }
    }
}

fn next_widget_id(widgets: &[DashboardWidget]) -> u64 {
    widgets.iter().map(|w| w.id.0 + 1).max().unwrap_or(1).max(1)
}

fn next_connector_id(connectors: &[Connector]) -> u64 {
    connectors
        .iter()
        .map(|c| c.id.0 + 1)
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
        };
        let blob = serde_json::to_string(&cfg).unwrap();
        let back: DashboardPanelConfig = serde_json::from_str(&blob).unwrap();
        assert_eq!(back.connectors.len(), 1);
        assert_eq!(back.connectors[0].style.label, "feed");
        assert!(back.connectors[0].style.on_top);
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
            WidgetKind::text(),
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

        assert_eq!(cfg.connectors.len(), 3);
        let schematic = &cfg.connectors[0];
        assert_eq!(schematic.points.len(), 3, "waypoints must survive");
        assert_eq!(schematic.style.shape, LineShape::Orthogonal);
        assert_eq!(schematic.style.arrow, ArrowEnds::End);
        assert!(!schematic.style.on_top);
        assert_eq!(
            schematic.style.bind.as_ref().unwrap().component,
            "sat1.wheels.wheels.0.arm"
        );

        let leader = &cfg.connectors[1];
        assert_eq!(leader.style.shape, LineShape::Curved);
        assert!(leader.style.on_top && leader.style.dashed);
        assert_eq!(leader.style.arrow, ArrowEnds::Both);
        assert!(leader.style.color.is_some());
        assert!(matches!(
            leader.points[0],
            ConnectorAnchor::Widget {
                side: Side::Top,
                ..
            }
        ));
        assert!(matches!(leader.points[1], ConnectorAnchor::Free { .. }));

        assert_eq!(cfg.connectors[2].style.shape, LineShape::Straight);

        // And every connector resolves against the widgets it shipped with.
        for c in &cfg.connectors {
            assert!(
                connectors::resolve_all(c, &cfg.widgets).is_some(),
                "connector {:?} did not resolve",
                c.id
            );
        }
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
        assert_eq!(next_connector_id(&[line(7), line(3)]), 8);
    }

    #[test]
    fn an_empty_dashboard_starts_at_one() {
        assert_eq!(next_connector_id(&[]), 1);
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
    let next_connector_id = next_connector_id(&cfg.connectors);

    cx.new(|_cx| DashboardPanel {
        db,
        title: if cfg.title.is_empty() {
            SharedString::from("Dashboard")
        } else {
            SharedString::from(cfg.title)
        },
        widgets: cfg.widgets,
        widget_live,
        connectors: cfg.connectors,
        connector_live: HashMap::new(),
        next_connector_id,
        selected_connector: None,
        tool: connectors::Tool::default(),
        draft: connectors::Draft::default(),
        next_id,
        editing: false,
        selected: None,
        container_bounds: None,
        scroll_offset: point(0.0, 0.0),
    })
}
