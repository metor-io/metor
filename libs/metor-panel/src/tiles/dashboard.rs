use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    AnyElement, AnyView, App, Bounds, Context, Corners, DragMoveEvent, Empty, Entity, IntoElement,
    Pixels, Point, Render, RenderImage, SharedString, Window, canvas, div, fill, point, prelude::*,
    px, size,
};
use image::{Frame, ImageBuffer, Rgba};
use metor_db::DB;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::command_palette::{PaletteAction, PaletteItem, PalettePage};
use crate::elements::viewer_3d::Viewer3d;
use crate::elements::{ComponentText, Monitor, Scrollbar, TimeSeriesPlot, new_component_table};
use crate::theme::theme;

use super::item::PaneItem;

const SNAP_GRID_PX: f32 = 10.0;
const MIN_WIDGET_PX: f32 = 40.0;
/// Width of the edge resize zones (like macOS window borders).
const EDGE_ZONE_PX: f32 = 6.0;
/// Size of corner resize zones.
const CORNER_ZONE_PX: f32 = 10.0;

/// Unique identifier for a widget within a dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WidgetId(pub u64);

/// Position and size in pixels on the dashboard canvas.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WidgetRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

fn snap_px(v: f32) -> f32 {
    (v / SNAP_GRID_PX).round() * SNAP_GRID_PX
}

/// The type of content a widget displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WidgetKind {
    Plot,
    Text,
    Table,
    Image,
    Monitor,
    Viewer3d,
}

impl WidgetKind {
    fn default_size(self) -> (f32, f32) {
        match self {
            WidgetKind::Plot => (400.0, 250.0),
            WidgetKind::Text => (160.0, 60.0),
            WidgetKind::Table => (400.0, 300.0),
            WidgetKind::Image => (300.0, 200.0),
            WidgetKind::Monitor => (150.0, 80.0),
            WidgetKind::Viewer3d => (480.0, 320.0),
        }
    }
}

/// Which edge or corner a resize drag started from.
#[derive(Debug, Clone, Copy)]
enum ResizeEdge {
    TopLeft,
    Top,
    TopRight,
    Left,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

/// A positioned widget on the dashboard canvas.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardWidget {
    pub id: WidgetId,
    pub rect: WidgetRect,
    pub kind: WidgetKind,
    pub config: serde_json::Value,
}

/// Drag payload when moving a widget.
struct DraggedWidget {
    widget_id: WidgetId,
    dashboard: Entity<DashboardPanel>,
    /// Fractional offset from the widget's origin to the grab point.
    grab_offset: Point<f32>,
}

impl Render for DraggedWidget {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// Drag payload when resizing a widget.
struct ResizingWidget {
    widget_id: WidgetId,
    dashboard: Entity<DashboardPanel>,
    edge: ResizeEdge,
    original_rect: WidgetRect,
}

impl Render for ResizingWidget {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// Free-form canvas dashboard where widgets are positioned at pixel coordinates.
pub struct DashboardPanel {
    db: Arc<DB>,
    self_entity: Entity<DashboardPanel>,
    title: SharedString,
    widgets: Vec<DashboardWidget>,
    widget_views: HashMap<WidgetId, AnyView>,
    widget_entities: HashMap<WidgetId, gpui::AnyEntity>,

    next_id: u64,
    editing: bool,
    selected: Option<WidgetId>,
    container_bounds: Option<Bounds<Pixels>>,
    /// Viewport pan offset in pixels. Negative values scroll the canvas
    /// so content below/right of the origin becomes visible.
    scroll_offset: Point<f32>,
}

impl DashboardPanel {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        Self {
            db,
            self_entity: cx.entity().clone(),
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

    /// Find an unoccupied position for a new widget of the given size.
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

    fn add_widget(
        &mut self,
        kind: WidgetKind,
        config: serde_json::Value,
        cx: &mut Context<Self>,
    ) -> WidgetId {
        let id = self.alloc_id();
        let (w, h) = kind.default_size();
        let rect = self.auto_place(w, h);
        let widget = DashboardWidget {
            id,
            rect,
            kind,
            config: config.clone(),
        };
        let (view, widget_entity) = create_widget_view(kind, &config, &self.db, cx);
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
                    create_widget_view(widget.kind, &widget.config, &self.db, cx);
                self.widget_views.insert(widget.id, view);
                self.widget_entities.insert(widget.id, widget_entity);
            }
        }
    }

    /// The furthest right and bottom edges of any widget.
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

    /// Clamp scroll offset so the viewport doesn't scroll past content bounds.
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

    /// Convert a window-space pixel position to canvas coordinates,
    /// accounting for the current scroll offset.
    fn pixel_to_canvas(&self, pixel: Point<Pixels>) -> Option<Point<f32>> {
        let bounds = self.container_bounds?;
        let x = f32::from(pixel.x - bounds.origin.x) - self.scroll_offset.x;
        let y = f32::from(pixel.y - bounds.origin.y) - self.scroll_offset.y;
        Some(point(x, y))
    }

    fn handle_widget_drag_move(
        &mut self,
        event: &DragMoveEvent<DraggedWidget>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let drag = event.drag(cx);
        let widget_id = drag.widget_id;
        let grab_offset = drag.grab_offset;

        let Some(pos) = self.pixel_to_canvas(event.event.position) else {
            return;
        };

        if let Some(w) = self.widgets.iter_mut().find(|w| w.id == widget_id) {
            w.rect.x = snap_px(pos.x - grab_offset.x).max(0.0);
            w.rect.y = snap_px(pos.y - grab_offset.y).max(0.0);
            cx.notify();
        }
    }

    fn handle_widget_resize_move(
        &mut self,
        event: &DragMoveEvent<ResizingWidget>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let drag = event.drag(cx);
        let widget_id = drag.widget_id;
        let edge = drag.edge;
        let orig = drag.original_rect;

        let Some(pos) = self.pixel_to_canvas(event.event.position) else {
            return;
        };

        if let Some(w) = self.widgets.iter_mut().find(|w| w.id == widget_id) {
            let snapped = point(snap_px(pos.x), snap_px(pos.y));
            let right = orig.x + orig.w;
            let bottom = orig.y + orig.h;

            match edge {
                ResizeEdge::Right => {
                    w.rect.w = (snapped.x - orig.x).max(MIN_WIDGET_PX);
                }
                ResizeEdge::Bottom => {
                    w.rect.h = (snapped.y - orig.y).max(MIN_WIDGET_PX);
                }
                ResizeEdge::Left => {
                    let new_x = snapped.x.min(right - MIN_WIDGET_PX).max(0.0);
                    w.rect.x = new_x;
                    w.rect.w = right - w.rect.x;
                }
                ResizeEdge::Top => {
                    let new_y = snapped.y.min(bottom - MIN_WIDGET_PX).max(0.0);
                    w.rect.y = new_y;
                    w.rect.h = bottom - w.rect.y;
                }
                ResizeEdge::BottomRight => {
                    w.rect.w = (snapped.x - orig.x).max(MIN_WIDGET_PX);
                    w.rect.h = (snapped.y - orig.y).max(MIN_WIDGET_PX);
                }
                ResizeEdge::BottomLeft => {
                    let new_x = snapped.x.min(right - MIN_WIDGET_PX).max(0.0);
                    w.rect.x = new_x;
                    w.rect.w = right - w.rect.x;
                    w.rect.h = (snapped.y - orig.y).max(MIN_WIDGET_PX);
                }
                ResizeEdge::TopRight => {
                    w.rect.w = (snapped.x - orig.x).max(MIN_WIDGET_PX);
                    let new_y = snapped.y.min(bottom - MIN_WIDGET_PX).max(0.0);
                    w.rect.y = new_y;
                    w.rect.h = bottom - w.rect.y;
                }
                ResizeEdge::TopLeft => {
                    let new_x = snapped.x.min(right - MIN_WIDGET_PX).max(0.0);
                    let new_y = snapped.y.min(bottom - MIN_WIDGET_PX).max(0.0);
                    w.rect.x = new_x;
                    w.rect.y = new_y;
                    w.rect.w = right - w.rect.x;
                    w.rect.h = bottom - w.rect.y;
                }
            }
            cx.notify();
        }
    }

    fn render_widget(&self, widget: &DashboardWidget, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme(cx);
        let view = self.widget_views.get(&widget.id);
        let r = &widget.rect;

        let mut container = div()
            .id(("dashboard-widget", widget.id.0 as usize))
            .absolute()
            .top(px(r.y + self.scroll_offset.y))
            .left(px(r.x + self.scroll_offset.x))
            .w(px(r.w))
            .h(px(r.h))
            .overflow_hidden()
            .border_1()
            .border_color(theme.border_primary)
            .rounded(px(4.0))
            .bg(theme.bg_primary);

        if let Some(view) = view {
            container = container.child(view.clone());
        }

        if self.editing {
            let widget_id = widget.id;
            let entity = self.self_entity.clone();
            let widget_rect = widget.rect;

            // Full-size interaction blocker — absorbs all clicks so the
            // inner widget content cannot be interacted with in edit mode.
            // Right-click opens the widget's inspector.
            // The move-zone drag and edge-zone resizes are layered above.
            let blocker_entity = self.self_entity.clone();
            let blocker_widget_id = widget.id;
            container = container.child(
                div()
                    .id(("widget-blocker", widget.id.0 as usize))
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, _| {})
                    .on_mouse_down(
                        gpui::MouseButton::Right,
                        move |event: &gpui::MouseDownEvent, window, cx| {
                            let pos = event.position;
                            blocker_entity.update(cx, |this, cx| {
                                this.open_widget_inspector(blocker_widget_id, pos, window, cx);
                            });
                        },
                    ),
            );

            // Center move zone — inset from edges so resize zones sit on top.
            container = container.child(
                div()
                    .id(("widget-move-zone", widget.id.0 as usize))
                    .absolute()
                    .top(px(EDGE_ZONE_PX))
                    .left(px(EDGE_ZONE_PX))
                    .right(px(EDGE_ZONE_PX))
                    .bottom(px(EDGE_ZONE_PX))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .on_drag(
                        DraggedWidget {
                            widget_id,
                            dashboard: entity.clone(),
                            grab_offset: point(0.0, 0.0),
                        },
                        {
                            let entity = entity.clone();
                            move |drag, _, window, cx| {
                                let grab_offset = entity
                                    .read(cx)
                                    .pixel_to_canvas(window.mouse_position())
                                    .map(|pos| point(pos.x - widget_rect.x, pos.y - widget_rect.y))
                                    .unwrap_or(point(0.0, 0.0));

                                cx.new(|_| DraggedWidget {
                                    widget_id: drag.widget_id,
                                    dashboard: drag.dashboard.clone(),
                                    grab_offset: grab_offset,
                                })
                            }
                        },
                    ),
            );

            // Edge resize zones — always present, like macOS window borders.
            container = self.render_edge_zones(container, widget, cx);
        }

        container.into_any_element()
    }

    /// Render invisible edge/corner zones around the widget border that
    /// initiate resize on drag, like macOS window edges.
    fn render_edge_zones(
        &self,
        mut container: gpui::Stateful<gpui::Div>,
        widget: &DashboardWidget,
        _cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let e = EDGE_ZONE_PX;
        let c = CORNER_ZONE_PX;

        // (edge, top, left, right, bottom, width, height, cursor)
        // Edges: thin strips along each side, inset from corners.
        // Corners: small squares at each corner.
        struct Zone {
            edge: ResizeEdge,
            top: Option<f32>,
            left: Option<f32>,
            right: Option<f32>,
            bottom: Option<f32>,
            width: ZoneDim,
            height: ZoneDim,
            cursor: gpui::CursorStyle,
        }

        enum ZoneDim {
            Px(f32),
            Fill, // stretch between the two corner zones
        }

        let zones = [
            // Corners
            Zone {
                edge: ResizeEdge::TopLeft,
                top: Some(0.0),
                left: Some(0.0),
                right: None,
                bottom: None,
                width: ZoneDim::Px(c),
                height: ZoneDim::Px(c),
                cursor: gpui::CursorStyle::ResizeUpLeftDownRight,
            },
            Zone {
                edge: ResizeEdge::TopRight,
                top: Some(0.0),
                left: None,
                right: Some(0.0),
                bottom: None,
                width: ZoneDim::Px(c),
                height: ZoneDim::Px(c),
                cursor: gpui::CursorStyle::ResizeUpRightDownLeft,
            },
            Zone {
                edge: ResizeEdge::BottomLeft,
                top: None,
                left: Some(0.0),
                right: None,
                bottom: Some(0.0),
                width: ZoneDim::Px(c),
                height: ZoneDim::Px(c),
                cursor: gpui::CursorStyle::ResizeUpRightDownLeft,
            },
            Zone {
                edge: ResizeEdge::BottomRight,
                top: None,
                left: None,
                right: Some(0.0),
                bottom: Some(0.0),
                width: ZoneDim::Px(c),
                height: ZoneDim::Px(c),
                cursor: gpui::CursorStyle::ResizeUpLeftDownRight,
            },
            // Edges (between corners)
            Zone {
                edge: ResizeEdge::Top,
                top: Some(0.0),
                left: Some(c),
                right: Some(c),
                bottom: None,
                width: ZoneDim::Fill,
                height: ZoneDim::Px(e),
                cursor: gpui::CursorStyle::ResizeRow,
            },
            Zone {
                edge: ResizeEdge::Bottom,
                top: None,
                left: Some(c),
                right: Some(c),
                bottom: Some(0.0),
                width: ZoneDim::Fill,
                height: ZoneDim::Px(e),
                cursor: gpui::CursorStyle::ResizeRow,
            },
            Zone {
                edge: ResizeEdge::Left,
                top: Some(c),
                left: Some(0.0),
                right: None,
                bottom: Some(c),
                width: ZoneDim::Px(e),
                height: ZoneDim::Fill,
                cursor: gpui::CursorStyle::ResizeColumn,
            },
            Zone {
                edge: ResizeEdge::Right,
                top: Some(c),
                left: None,
                right: Some(0.0),
                bottom: Some(c),
                width: ZoneDim::Px(e),
                height: ZoneDim::Fill,
                cursor: gpui::CursorStyle::ResizeColumn,
            },
        ];

        for (ix, zone) in zones.iter().enumerate() {
            let widget_id = widget.id;
            let original_rect = widget.rect;
            let dashboard = self.self_entity.clone();
            let edge = zone.edge;

            let mut handle = div()
                .id(("edge-zone", widget.id.0 as usize * 10 + ix))
                .absolute()
                .cursor(zone.cursor);

            if let Some(v) = zone.top {
                handle = handle.top(px(v));
            }
            if let Some(v) = zone.left {
                handle = handle.left(px(v));
            }
            if let Some(v) = zone.right {
                handle = handle.right(px(v));
            }
            if let Some(v) = zone.bottom {
                handle = handle.bottom(px(v));
            }

            handle = match zone.width {
                ZoneDim::Px(v) => handle.w(px(v)),
                ZoneDim::Fill => handle, // width determined by left+right anchors
            };
            handle = match zone.height {
                ZoneDim::Px(v) => handle.h(px(v)),
                ZoneDim::Fill => handle, // height determined by top+bottom anchors
            };

            handle = handle.on_drag(
                ResizingWidget {
                    widget_id,
                    dashboard: dashboard.clone(),
                    edge,
                    original_rect,
                },
                move |drag, _, _, cx| {
                    cx.new(|_| ResizingWidget {
                        widget_id: drag.widget_id,
                        dashboard: drag.dashboard.clone(),
                        edge: drag.edge,
                        original_rect: drag.original_rect,
                    })
                },
            );

            container = container.child(handle);
        }

        container
    }

    fn render_grid_overlay(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let grid_color = theme.border_primary.opacity(0.3);
        let bounds = self.container_bounds;
        let offset = self.scroll_offset;

        let grid_step = SNAP_GRID_PX;
        canvas(
            move |_, _, _| {},
            move |_, _, window, _| {
                let Some(bounds) = bounds else { return };
                let w = f32::from(bounds.size.width);
                let h = f32::from(bounds.size.height);

                // Offset grid lines by scroll so they pan with content
                let start_x = offset.x.rem_euclid(grid_step);
                let mut x_off = start_x;
                while x_off < w {
                    let origin = point(bounds.origin.x + px(x_off), bounds.origin.y);
                    let sz = size(px(1.0), bounds.size.height);
                    window.paint_quad(fill(Bounds { origin, size: sz }, grid_color));
                    x_off += grid_step;
                }

                let start_y = offset.y.rem_euclid(grid_step);
                let mut y_off = start_y;
                while y_off < h {
                    let origin = point(bounds.origin.x, bounds.origin.y + px(y_off));
                    let sz = size(bounds.size.width, px(1.0));
                    window.paint_quad(fill(Bounds { origin, size: sz }, grid_color));
                    y_off += grid_step;
                }
            },
        )
        .size_full()
        .absolute()
    }
}

/// Build the command palette page for dashboard operations.
pub fn dashboard_palette_page(
    dashboard: Entity<DashboardPanel>,
    db: Arc<DB>,
    cx: &App,
) -> PalettePage {
    let mut items = vec![];

    items.push(PaletteItem::new("Add Widget", {
        let dashboard = dashboard.clone();
        let db = db.clone();
        PaletteAction::NextPage {
            label: Some("Add".into()),
            page: Box::new(move || add_widget_page(dashboard.clone(), db.clone())),
        }
    }));

    let editing = dashboard.read(cx).editing;
    let edit_label = if editing {
        "Exit Edit Mode"
    } else {
        "Enter Edit Mode"
    };
    items.push(PaletteItem::new(edit_label, {
        let dashboard = dashboard.clone();
        PaletteAction::Execute(Box::new(move |_, _, cx| {
            dashboard.update(cx, |this, cx| {
                this.editing = !this.editing;
                if !this.editing {
                    this.selected = None;
                }
                cx.notify();
            });
        }))
    }));

    let widgets = &dashboard.read(cx).widgets;
    if !widgets.is_empty() {
        let widget_infos: Vec<(WidgetId, SharedString)> = widgets
            .iter()
            .map(|w| (w.id, widget_display_label(w)))
            .collect();

        items.push(PaletteItem::new("Remove Widget", {
            let dashboard = dashboard.clone();
            PaletteAction::NextPage {
                label: Some("Remove".into()),
                page: Box::new(move || {
                    let remove_items: Vec<PaletteItem> = widget_infos
                        .iter()
                        .map(|(widget_id, label)| {
                            let widget_id = *widget_id;
                            let dashboard = dashboard.clone();
                            PaletteItem::new(
                                label.clone(),
                                PaletteAction::Execute(Box::new(move |_, _, cx| {
                                    dashboard.update(cx, |this, cx| {
                                        this.remove_widget(widget_id, cx);
                                    });
                                })),
                            )
                        })
                        .collect();
                    PalettePage::new(remove_items).prompt("Select widget to remove")
                }),
            }
        }));
    }

    // Z-order controls when a widget is selected
    let selected = dashboard.read(cx).selected;
    if let Some(sel_id) = selected {
        items.push(PaletteItem::new("Bring to Front", {
            let dashboard = dashboard.clone();
            PaletteAction::Execute(Box::new(move |_, _, cx| {
                dashboard.update(cx, |this, cx| {
                    this.bring_to_front(sel_id, cx);
                });
            }))
        }));
        items.push(PaletteItem::new("Send to Back", {
            let dashboard = dashboard.clone();
            PaletteAction::Execute(Box::new(move |_, _, cx| {
                dashboard.update(cx, |this, cx| {
                    this.send_to_back(sel_id, cx);
                });
            }))
        }));
    }

    items.push(PaletteItem::new("Rename", {
        let dashboard = dashboard.clone();
        PaletteAction::NextPage {
            label: Some("Rename".into()),
            page: Box::new(move || {
                let dashboard = dashboard.clone();
                let apply = PaletteItem::new(
                    "Apply",
                    PaletteAction::Execute(Box::new(move |input, _, cx| {
                        if !input.is_empty() {
                            dashboard.update(cx, |this, cx| {
                                this.title = SharedString::from(input.to_string());
                                cx.notify();
                            });
                        }
                    })),
                );
                PalettePage::new(vec![])
                    .prompt("Dashboard name...")
                    .default_action(apply)
            }),
        }
    }));

    PalettePage::new(items).label("Dashboard")
}

fn add_widget_page(dashboard: Entity<DashboardPanel>, db: Arc<DB>) -> PalettePage {
    let items = vec![
        PaletteItem::new("Time Series Plot", {
            let dashboard = dashboard.clone();
            PaletteAction::Execute(Box::new(move |_, _, cx| {
                dashboard.update(cx, |this, cx| {
                    this.add_widget(WidgetKind::Plot, serde_json::json!({}), cx);
                });
            }))
        }),
        PaletteItem::new("Component Text", {
            let dashboard = dashboard.clone();
            let db = db.clone();
            PaletteAction::NextPage {
                label: Some("Text".into()),
                page: Box::new(move || {
                    component_picker_for_widget(dashboard.clone(), db.clone(), WidgetKind::Text)
                }),
            }
        }),
        PaletteItem::new("Component Table", {
            let dashboard = dashboard.clone();
            PaletteAction::Execute(Box::new(move |_, _, cx| {
                dashboard.update(cx, |this, cx| {
                    this.add_widget(WidgetKind::Table, serde_json::json!({}), cx);
                });
            }))
        }),
        PaletteItem::new("Monitor", {
            let dashboard = dashboard.clone();
            let db = db.clone();
            PaletteAction::NextPage {
                label: Some("Monitor".into()),
                page: Box::new(move || {
                    component_picker_for_widget(dashboard.clone(), db.clone(), WidgetKind::Monitor)
                }),
            }
        }),
        PaletteItem::new("Image", {
            let dashboard = dashboard.clone();
            PaletteAction::NextPage {
                label: Some("Image".into()),
                page: Box::new(move || image_path_page(dashboard.clone())),
            }
        }),
        PaletteItem::new("3D Viewer", {
            let dashboard = dashboard.clone();
            PaletteAction::Execute(Box::new(move |_, _, cx| {
                dashboard.update(cx, |this, cx| {
                    this.add_widget(WidgetKind::Viewer3d, serde_json::json!({}), cx);
                });
            }))
        }),
    ];

    PalettePage::new(items).prompt("Select widget type")
}

fn component_picker_for_widget(
    dashboard: Entity<DashboardPanel>,
    db: Arc<DB>,
    kind: WidgetKind,
) -> PalettePage {
    let items: Vec<PaletteItem> = crate::trace_picker::list_components(&db)
        .into_iter()
        .map(|(_id, name)| {
            let dashboard = dashboard.clone();
            let name_clone = name.clone();
            PaletteItem::new(
                SharedString::from(name),
                PaletteAction::Execute(Box::new(move |_, _, cx| {
                    let config = serde_json::json!({ "component": name_clone });
                    dashboard.update(cx, |this, cx| {
                        this.add_widget(kind, config, cx);
                    });
                })),
            )
        })
        .collect();

    PalettePage::new(items).prompt("Select component")
}

fn image_path_page(dashboard: Entity<DashboardPanel>) -> PalettePage {
    let apply = PaletteItem::new(
        "Add Image",
        PaletteAction::Execute(Box::new(move |input, _, cx| {
            if !input.is_empty() {
                let config = serde_json::json!({ "path": input });
                dashboard.update(cx, |this, cx| {
                    this.add_widget(WidgetKind::Image, config, cx);
                });
            }
        })),
    );
    PalettePage::new(vec![])
        .prompt("Image file path...")
        .default_action(apply)
}

fn widget_display_label(widget: &DashboardWidget) -> SharedString {
    let component_label = || {
        widget
            .config
            .get("component")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
    };
    match widget.kind {
        WidgetKind::Plot => SharedString::from(format!("Plot #{}", widget.id.0)),
        WidgetKind::Text => SharedString::from(format!("Text: {}", component_label())),
        WidgetKind::Table => SharedString::from(format!("Table #{}", widget.id.0)),
        WidgetKind::Image => SharedString::from(format!(
            "Image: {}",
            widget
                .config
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
        )),
        WidgetKind::Monitor => SharedString::from(format!("Monitor: {}", component_label())),
        WidgetKind::Viewer3d => SharedString::from(format!("3D Viewer #{}", widget.id.0)),
    }
}

fn create_widget_view(
    kind: WidgetKind,
    config: &serde_json::Value,
    db: &Arc<DB>,
    cx: &mut App,
) -> (AnyView, gpui::AnyEntity) {
    macro_rules! widget {
        ($entity:expr) => {{
            let e = $entity;
            let any = e.clone().into_any();
            (AnyView::from(e), any)
        }};
    }
    match kind {
        WidgetKind::Plot => {
            widget!(cx.new(|cx| TimeSeriesPlot::new(db.clone(), vec![], cx)))
        }
        WidgetKind::Text => {
            let component_name = config
                .get("component")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let component_id = db.with_state(|state| {
                state
                    .component_metadata_iter()
                    .find(|(_, meta)| meta.name == component_name)
                    .map(|(id, _)| *id)
            });
            if let Some(id) = component_id {
                widget!(cx.new(|cx| ComponentText::new(db.clone(), id, cx)))
            } else {
                widget!(cx.new(|_cx| PlaceholderWidget {
                    label: SharedString::from(format!("? {}", component_name)),
                }))
            }
        }
        WidgetKind::Table => {
            widget!(cx.new(|cx| new_component_table(db.clone(), cx)))
        }
        WidgetKind::Image => {
            let path = config
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            widget!(cx.new(|_cx| ImageWidget::load(path)))
        }
        WidgetKind::Monitor => {
            let component_name = config
                .get("component")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let component_id = db.with_state(|state| {
                state
                    .component_metadata_iter()
                    .find(|(_, meta)| meta.name == component_name)
                    .map(|(id, _)| *id)
            });
            if let Some(id) = component_id {
                widget!(cx.new(|cx| Monitor::new(db.clone(), id, cx)))
            } else {
                widget!(cx.new(|_cx| PlaceholderWidget {
                    label: SharedString::from(format!("? {}", component_name)),
                }))
            }
        }
        WidgetKind::Viewer3d => {
            widget!(cx.new(|cx| Viewer3d::with_db(db.clone(), cx)))
        }
    }
}

/// Renders an image loaded from a file path, scaled to fill its widget bounds.
struct ImageWidget {
    render_image: Option<Arc<RenderImage>>,
    label: SharedString,
}

impl ImageWidget {
    fn load(path: String) -> Self {
        let render_image = std::fs::read(&path)
            .ok()
            .and_then(|bytes| image::load_from_memory(&bytes).ok())
            .map(|img| {
                let rgba = img.to_rgba8();
                let (w, h) = rgba.dimensions();
                let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(w, h, rgba.into_raw())
                    .expect("buffer size mismatch");
                let frames = SmallVec::from_elem(Frame::new(buffer), 1);
                Arc::new(RenderImage::new(frames))
            });

        let label = if render_image.is_some() {
            SharedString::from(path)
        } else {
            SharedString::from(format!("Failed to load: {}", path))
        };

        Self {
            render_image,
            label,
        }
    }
}

impl Render for ImageWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(img) = &self.render_image {
            let img = img.clone();
            div().size_full().child(
                canvas(
                    move |_, _, _| {},
                    move |bounds, _, window, _| {
                        let _ =
                            window.paint_image(bounds, Corners::default(), img.clone(), 0, false);
                    },
                )
                .size_full(),
            )
        } else {
            let theme = theme(cx);
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.text_tertiary)
                .text_size(px(14.0))
                .child(self.label.clone())
        }
    }
}

/// Placeholder shown when a referenced component isn't available.
struct PlaceholderWidget {
    label: SharedString,
}

impl Render for PlaceholderWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(theme.text_tertiary)
            .text_size(px(14.0))
            .child(self.label.clone())
    }
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

        // Grid overlay in edit mode
        if self.editing {
            canvas_div = canvas_div.child(self.render_grid_overlay(cx));
        }

        // Render widgets in order (z-order = vec order)
        let widgets: Vec<DashboardWidget> = self.widgets.clone();
        for widget in &widgets {
            canvas_div = canvas_div.child(self.render_widget(widget, cx));
        }

        // Drag-move handler on the canvas
        canvas_div = canvas_div
            .on_drag_move(cx.listener(Self::handle_widget_drag_move))
            .on_drag_move(cx.listener(Self::handle_widget_resize_move));

        // Scroll wheel pans the viewport. Child widgets that handle scroll
        // themselves (e.g. plot zoom) should call cx.stop_propagation() to
        // prevent this handler from also firing.
        canvas_div = canvas_div.on_scroll_wheel(cx.listener(
            |this, event: &gpui::ScrollWheelEvent, _window, cx| {
                let delta = event.delta.pixel_delta(px(20.0));
                this.scroll_offset.x += f32::from(delta.x);
                this.scroll_offset.y += f32::from(delta.y);
                this.clamp_scroll();
                cx.notify();
            },
        ));

        // Click on empty canvas space deselects.
        // Uses on_click (requires .id()) which only fires when this element
        // is the actual click target, not when a child widget is clicked.
        if self.editing {
            let entity = self.self_entity.clone();
            canvas_div = canvas_div.on_click(move |_, _, cx| {
                entity.update(cx, |this, cx| {
                    this.selected = None;
                    cx.notify();
                });
            });
        }

        // Edit mode indicator pill
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

        // Empty state
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

        // Scrollbars
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

    fn serialize(&self, _cx: &App) -> serde_json::Value {
        serde_json::json!({
            "title": self.title.as_ref(),
            "next_id": self.next_id,
            "widgets": self.widgets,
        })
    }
}

/// Deserialize a DashboardPanel from its serialized JSON state.
pub fn deserialize_dashboard(
    db: Arc<DB>,
    value: serde_json::Value,
    cx: &mut App,
) -> Entity<DashboardPanel> {
    let title = value
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("Dashboard")
        .to_string();
    let next_id = value.get("next_id").and_then(|v| v.as_u64()).unwrap_or(1);
    let widgets: Vec<DashboardWidget> = value
        .get("widgets")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let mut widget_views = HashMap::new();
    let mut widget_entities = HashMap::new();
    for widget in &widgets {
        let (view, widget_entity) = create_widget_view(widget.kind, &widget.config, &db, cx);
        widget_views.insert(widget.id, view);
        widget_entities.insert(widget.id, widget_entity);
    }

    cx.new(|cx| DashboardPanel {
        db,
        self_entity: cx.entity().clone(),
        title: SharedString::from(title),
        widgets,
        widget_views,
        widget_entities,
        next_id,
        editing: false,
        selected: None,
        container_bounds: None,
        scroll_offset: point(0.0, 0.0),
    })
}
