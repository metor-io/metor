//! Edit-mode drag and resize affordances for dashboard widgets.
//!
//! Houses the drag payload types and the per-widget chrome that renders
//! move/resize zones when the dashboard is editable.
use gpui::{
    AnyElement, Context, DragMoveEvent, Empty, Entity, IntoElement, Render, Window, div, point,
    prelude::*, px,
};

use crate::theme::theme;

use super::{DashboardPanel, DashboardWidget, WidgetId, WidgetRect, snap_px};

const MIN_WIDGET_PX: f32 = 40.0;
const EDGE_ZONE_PX: f32 = 6.0;
const CORNER_ZONE_PX: f32 = 10.0;

/// Edge or corner a resize drag grabbed.
#[derive(Debug, Clone, Copy)]
pub(super) enum ResizeEdge {
    TopLeft,
    Top,
    TopRight,
    Left,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

/// Payload carried while a widget is being translated.
///
/// `grab_offset` is the cursor position relative to the widget's top-left
/// so the drag handler can move the widget without snapping the cursor to
/// the origin.
pub(super) struct DraggedWidget {
    pub(super) widget_id: WidgetId,
    pub(super) dashboard: Entity<DashboardPanel>,
    pub(super) grab_offset: gpui::Point<f32>,
}

impl Render for DraggedWidget {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// Payload carried while a widget is being resized; `original_rect` lets
/// the handler compute deltas against the drag start rather than
/// accumulating rounding error.
pub(super) struct ResizingWidget {
    pub(super) widget_id: WidgetId,
    pub(super) dashboard: Entity<DashboardPanel>,
    pub(super) edge: ResizeEdge,
    pub(super) original_rect: WidgetRect,
}

impl Render for ResizingWidget {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

impl DashboardPanel {
    pub(super) fn handle_widget_drag_move(
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

    pub(super) fn handle_widget_resize_move(
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

    pub(super) fn render_widget(
        &self,
        widget: &DashboardWidget,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
            let widget_rect = widget.rect;
            // Edit-mode blocker: swallows inner-view clicks so drag/resize
            // zones are the only interactive surface. Right-click still
            // opens the widget's inspector.
            let blocker_entity = cx.entity();
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

            let entity = cx.entity();
            // Inner move zone is inset so resize zones along the edges
            // remain hit-testable on top.
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
                                    grab_offset,
                                })
                            }
                        },
                    ),
            );

            container = self.render_edge_zones(container, widget, cx);
        }

        container.into_any_element()
    }

    /// Add invisible 8-point resize zones that ring the widget's border.
    fn render_edge_zones(
        &self,
        mut container: gpui::Stateful<gpui::Div>,
        widget: &DashboardWidget,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let e = EDGE_ZONE_PX;
        let c = CORNER_ZONE_PX;

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
            /// Stretch between the two bracketing corner zones.
            Fill,
        }

        let zones = [
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
            let dashboard = cx.entity();
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
                ZoneDim::Fill => handle,
            };
            handle = match zone.height {
                ZoneDim::Px(v) => handle.h(px(v)),
                ZoneDim::Fill => handle,
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
}
