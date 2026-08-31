//! Schematic connectors: the lines that make a dashboard a diagram.
//!
//! A P&ID, a circuit, a signal-flow sketch — all of them are boxes plus the
//! lines between them, and until now the dashboard could only draw the boxes.
//! A [`Connector`] is a polyline over two or more [`ConnectorAnchor`]s, where
//! an anchor is either a free point on the canvas or *a side of a widget*.
//!
//! Anchoring to widgets is the reason connectors live inside the dashboard
//! canvas rather than as a window-level overlay across tiles: an anchor
//! resolves against the widget's live rect every frame, so a line follows its
//! endpoints through drags and resizes without bookkeeping.
//!
//! Two properties carry most of the value:
//!
//! - [`ConnectorStyle::on_top`] decides whether a line paints under the
//!   widgets or over them. Under is right for a schematic — a pipe should
//!   disappear into the box it enters. Over is right for a callout leader
//!   from a diagram to a live readout. One flag, both jobs.
//! - [`ConnectorBinding`] colours a line from telemetry, so a pipe shows
//!   flow and a bus shows power. That is the difference between a picture of
//!   the vehicle and a display of it.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{Hsla, Pixels, Point, point, px};
use metor_db::DB;
use metor_proto::types::ComponentId;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use super::{DashboardWidget, WidgetId, WidgetRect};
use crate::graph_canvas::LineStyle;

/// How a connector's points are joined.
///
/// Defined here rather than beside the painting code because it is part of
/// the serialized connector config, so it has to stay publicly nameable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineShape {
    /// Straight segments between consecutive points.
    Straight,
    /// Right-angle elbows — how pipes and traces are drawn.
    #[default]
    Orthogonal,
    /// A smooth cubic chain, as the node editor draws its wires.
    Curved,
}

/// Monotonic id assigned to a connector when it is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConnectorId(pub u64);

/// Which edge of a widget an anchor rides.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Top,
    Right,
    #[default]
    Bottom,
    Left,
}

/// One end or waypoint of a connector.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ConnectorAnchor {
    /// A fixed point in canvas coordinates.
    Free { x: f32, y: f32 },
    /// A point on a widget's edge, `t` along that edge from its start.
    /// Re-resolved every frame, so it tracks the widget.
    Widget { id: WidgetId, side: Side, t: f32 },
}

/// Which ends of a connector get an arrowhead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArrowEnds {
    #[default]
    None,
    End,
    Both,
}

/// Telemetry that colours a connector.
///
/// `threshold` compares against the magnitude, so a signed rate or current
/// energizes the line in either direction.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectorBinding {
    pub component: String,
    pub element: usize,
    pub threshold: f64,
    pub on_color: Option<Hsla>,
}

/// How a connector is drawn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectorStyle {
    pub color: Option<Hsla>,
    pub width: f32,
    pub dashed: bool,
    pub shape: LineShape,
    pub arrow: ArrowEnds,
    pub label: String,
    /// Paint above the widgets instead of below them.
    pub on_top: bool,
    pub bind: Option<ConnectorBinding>,
}

impl Default for ConnectorStyle {
    fn default() -> Self {
        Self {
            color: None,
            width: 1.5,
            dashed: false,
            shape: LineShape::default(),
            arrow: ArrowEnds::default(),
            label: String::new(),
            on_top: false,
            bind: None,
        }
    }
}

/// A line across the dashboard canvas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Connector {
    pub id: ConnectorId,
    /// Two or more anchors; fewer draws nothing.
    pub points: Vec<ConnectorAnchor>,
    #[serde(default)]
    pub style: ConnectorStyle,
}

impl Connector {
    pub(crate) fn stroke(&self) -> LineStyle {
        LineStyle {
            width: px(self.style.width.max(0.5)),
            dashed: self.style.dashed,
            shape: self.style.shape,
        }
    }
}

/// What a click on the dashboard canvas does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Tool {
    #[default]
    Select,
    /// Each click drops an anchor; a double-click ends the run.
    DrawConnector,
}

/// A connector being drawn.
///
/// Click-to-place rather than drag-to-draw: it is the standard schematic
/// gesture, it allows more than two points, and it sidesteps the drag
/// handling the widget move/resize zones already own.
#[derive(Default)]
pub(super) struct Draft {
    pub points: Vec<ConnectorAnchor>,
    /// Last pointer position in canvas coordinates, so the run previews to
    /// the cursor before the next anchor lands.
    pub cursor: Option<(f32, f32)>,
}

/// Live state for one bound connector: whether its source currently reads
/// on, and the task keeping that answer fresh.
pub(super) struct ConnectorLive {
    pub on: bool,
    pub _task: gpui::Task<()>,
}

/// The point on a widget's edge at fraction `t`, in canvas coordinates.
pub(super) fn edge_point(rect: &WidgetRect, side: Side, t: f32) -> (f32, f32) {
    let t = t.clamp(0.0, 1.0);
    match side {
        Side::Top => (rect.x + rect.w * t, rect.y),
        Side::Bottom => (rect.x + rect.w * t, rect.y + rect.h),
        Side::Left => (rect.x, rect.y + rect.h * t),
        Side::Right => (rect.x + rect.w, rect.y + rect.h * t),
    }
}

/// The edge of `rect` nearest a canvas point, and how far along it to sit.
///
/// Used when a drawn anchor lands on a widget: attaching to the nearest edge
/// is what makes a click-to-place gesture feel like it snapped to the box
/// rather than to an arbitrary interior point.
pub(super) fn nearest_side(rect: &WidgetRect, at: (f32, f32)) -> (Side, f32) {
    let (x, y) = at;
    let gaps = [
        (Side::Left, (x - rect.x).abs()),
        (Side::Right, (x - (rect.x + rect.w)).abs()),
        (Side::Top, (y - rect.y).abs()),
        (Side::Bottom, (y - (rect.y + rect.h)).abs()),
    ];
    let (side, _) = gaps
        .into_iter()
        .fold((Side::Top, f32::INFINITY), |best, next| {
            if next.1 < best.1 { next } else { best }
        });
    let t = match side {
        Side::Top | Side::Bottom if rect.w > 0.0 => (x - rect.x) / rect.w,
        Side::Left | Side::Right if rect.h > 0.0 => (y - rect.y) / rect.h,
        _ => 0.5,
    };
    (side, t.clamp(0.0, 1.0))
}

/// Resolve one anchor against the current widget set.
///
/// A `Widget` anchor whose widget was deleted resolves to `None`; the
/// connector is then skipped rather than snapping to the origin.
pub(super) fn resolve(anchor: &ConnectorAnchor, widgets: &[DashboardWidget]) -> Option<(f32, f32)> {
    match anchor {
        ConnectorAnchor::Free { x, y } => Some((*x, *y)),
        ConnectorAnchor::Widget { id, side, t } => widgets
            .iter()
            .find(|w| w.id == *id)
            .map(|w| edge_point(&w.rect, *side, *t)),
    }
}

/// Resolve every anchor of a connector, or `None` if any is unresolvable.
pub(super) fn resolve_all(
    connector: &Connector,
    widgets: &[DashboardWidget],
) -> Option<SmallVec<[Point<Pixels>; 6]>> {
    if connector.points.len() < 2 {
        return None;
    }
    connector
        .points
        .iter()
        .map(|a| resolve(a, widgets).map(|(x, y)| point(px(x), px(y))))
        .collect()
}

/// Midpoint of a resolved run, where a connector's label sits.
pub(super) fn label_anchor(points: &[Point<Pixels>]) -> Point<Pixels> {
    // The middle *vertex* rather than the arc-length midpoint: cheap, stable
    // under drags, and for a two-point line it is the true middle anyway.
    match points.len() {
        0 => point(px(0.0), px(0.0)),
        2 => point(
            (points[0].x + points[1].x) / 2.0,
            (points[0].y + points[1].y) / 2.0,
        ),
        n => points[n / 2],
    }
}

/// Reconcile per-connector binding tasks against the current connector list.
///
/// Returns the tasks to install, keyed by id; the caller owns the map because
/// the tasks close over its entity. Connectors that lost their binding, or
/// vanished entirely, are dropped from `live` here — leaving a task behind
/// would keep streaming for the dashboard's whole lifetime.
pub(super) fn reconcile_bindings<F>(
    connectors: &[Connector],
    live: &mut HashMap<ConnectorId, ConnectorLive>,
    mut spawn: F,
) where
    F: FnMut(ConnectorId, ComponentId, usize) -> gpui::Task<()>,
{
    live.retain(|id, _| {
        connectors
            .iter()
            .any(|c| c.id == *id && c.style.bind.is_some())
    });
    for c in connectors {
        let Some(bind) = &c.style.bind else { continue };
        if live.contains_key(&c.id) {
            continue;
        }
        let task = spawn(c.id, ComponentId::new(&bind.component), bind.element);
        live.insert(
            c.id,
            ConnectorLive {
                on: false,
                _task: task,
            },
        );
    }
}

/// The colour a connector paints in this frame.
///
/// An unbound connector is simply its configured colour. A bound one reads
/// on in `on_color` and off as the base colour dimmed — the same hue, so the
/// line stays identifiable when it is inactive.
pub(super) fn line_color(
    connector: &Connector,
    live: Option<&ConnectorLive>,
    theme: &crate::theme::Theme,
) -> Hsla {
    let base = connector.style.color.unwrap_or(theme.line_color);
    match live {
        None => base,
        Some(state) if state.on => connector
            .style
            .bind
            .as_ref()
            .and_then(|b| b.on_color)
            .unwrap_or(theme.control_active),
        Some(_) => crate::theme::Theme::dim(base, 0.25),
    }
}

/// Spawn the stream task behind one binding.
pub(super) fn spawn_binding<E>(
    db: Arc<DB>,
    component: ComponentId,
    element: usize,
    threshold: f64,
    cx: &mut gpui::Context<E>,
    apply: impl Fn(&mut E, bool, &mut gpui::Context<E>) + Send + 'static,
) -> gpui::Task<()>
where
    E: 'static,
{
    crate::views::binding::spawn_scalar_stream(db, component, element, cx, move |view, v, cx| {
        apply(view, v.abs() > threshold, cx);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> WidgetRect {
        WidgetRect {
            x: 100.0,
            y: 50.0,
            w: 200.0,
            h: 80.0,
        }
    }

    #[test]
    fn edge_points_sit_on_their_edge() {
        let r = rect();
        assert_eq!(edge_point(&r, Side::Top, 0.0), (100.0, 50.0));
        assert_eq!(edge_point(&r, Side::Top, 1.0), (300.0, 50.0));
        assert_eq!(edge_point(&r, Side::Bottom, 0.5), (200.0, 130.0));
        assert_eq!(edge_point(&r, Side::Left, 0.5), (100.0, 90.0));
        assert_eq!(edge_point(&r, Side::Right, 1.0), (300.0, 130.0));
    }

    #[test]
    fn edge_fraction_clamps() {
        let r = rect();
        assert_eq!(edge_point(&r, Side::Top, -3.0), (100.0, 50.0));
        assert_eq!(edge_point(&r, Side::Top, 9.0), (300.0, 50.0));
    }

    #[test]
    fn a_click_snaps_to_the_edge_it_is_nearest() {
        let r = rect();
        // Just inside the left edge, vertically centred.
        let (side, t) = nearest_side(&r, (105.0, 90.0));
        assert_eq!(side, Side::Left);
        assert!((t - 0.5).abs() < 1e-5);
        // Near the top, horizontally a quarter across.
        let (side, t) = nearest_side(&r, (150.0, 53.0));
        assert_eq!(side, Side::Top);
        assert!((t - 0.25).abs() < 1e-5);
        // Near the bottom-right.
        let (side, _) = nearest_side(&r, (298.0, 125.0));
        assert_eq!(side, Side::Right);
    }

    #[test]
    fn a_snapped_anchor_lands_back_on_the_widget() {
        let r = rect();
        for at in [(105.0, 90.0), (150.0, 53.0), (298.0, 125.0), (200.0, 128.0)] {
            let (side, t) = nearest_side(&r, at);
            let (x, y) = edge_point(&r, side, t);
            let on_edge = (x - r.x).abs() < 1e-3
                || (x - (r.x + r.w)).abs() < 1e-3
                || (y - r.y).abs() < 1e-3
                || (y - (r.y + r.h)).abs() < 1e-3;
            assert!(on_edge, "{at:?} resolved off the rect: {x},{y}");
        }
    }

    #[test]
    fn a_degenerate_rect_does_not_divide_by_zero() {
        let r = WidgetRect {
            x: 10.0,
            y: 10.0,
            w: 0.0,
            h: 0.0,
        };
        let (_, t) = nearest_side(&r, (10.0, 10.0));
        assert!(t.is_finite());
    }

    fn widget(id: u64, rect: WidgetRect) -> DashboardWidget {
        DashboardWidget {
            id: WidgetId(id),
            rect,
            kind: super::super::WidgetKind::text(),
            config: "{}".into(),
            frame: true,
        }
    }

    #[test]
    fn widget_anchors_follow_their_widget() {
        let mut widgets = vec![widget(1, rect())];
        let anchor = ConnectorAnchor::Widget {
            id: WidgetId(1),
            side: Side::Top,
            t: 0.5,
        };
        assert_eq!(resolve(&anchor, &widgets), Some((200.0, 50.0)));
        // Move the widget; the anchor moves with it, no bookkeeping.
        widgets[0].rect.x += 40.0;
        widgets[0].rect.y += 10.0;
        assert_eq!(resolve(&anchor, &widgets), Some((240.0, 60.0)));
        // Resize it, too.
        widgets[0].rect.w = 100.0;
        assert_eq!(resolve(&anchor, &widgets), Some((190.0, 60.0)));
    }

    #[test]
    fn an_anchor_on_a_deleted_widget_unresolves() {
        let anchor = ConnectorAnchor::Widget {
            id: WidgetId(7),
            side: Side::Top,
            t: 0.5,
        };
        assert_eq!(resolve(&anchor, &[]), None);

        let connector = Connector {
            id: ConnectorId(1),
            points: vec![ConnectorAnchor::Free { x: 0.0, y: 0.0 }, anchor],
            style: ConnectorStyle::default(),
        };
        assert!(resolve_all(&connector, &[]).is_none());
    }

    #[test]
    fn a_connector_needs_two_anchors() {
        let connector = Connector {
            id: ConnectorId(1),
            points: vec![ConnectorAnchor::Free { x: 0.0, y: 0.0 }],
            style: ConnectorStyle::default(),
        };
        assert!(resolve_all(&connector, &[]).is_none());
    }

    #[test]
    fn free_anchors_resolve_to_themselves() {
        let connector = Connector {
            id: ConnectorId(1),
            points: vec![
                ConnectorAnchor::Free { x: 1.0, y: 2.0 },
                ConnectorAnchor::Free { x: 3.0, y: 4.0 },
            ],
            style: ConnectorStyle::default(),
        };
        let resolved = resolve_all(&connector, &[]).unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(f32::from(resolved[1].x), 3.0);
    }

    #[test]
    fn a_two_point_label_sits_at_the_middle() {
        let p = label_anchor(&[point(px(0.0), px(0.0)), point(px(10.0), px(20.0))]);
        assert_eq!((f32::from(p.x), f32::from(p.y)), (5.0, 10.0));
    }

    #[test]
    fn bindings_are_created_once_and_dropped_with_their_connector() {
        let mut live = HashMap::new();
        let bound = |id: u64| Connector {
            id: ConnectorId(id),
            points: vec![
                ConnectorAnchor::Free { x: 0.0, y: 0.0 },
                ConnectorAnchor::Free { x: 1.0, y: 1.0 },
            ],
            style: ConnectorStyle {
                bind: Some(ConnectorBinding {
                    component: "a.b".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        };

        // Cell, so counting spawns doesn't hold a mutable borrow across the
        // assertions between passes.
        let spawned = std::cell::Cell::new(0);
        let spawn = |_id, _c, _e| {
            spawned.set(spawned.get() + 1);
            gpui::Task::ready(())
        };

        let list = vec![bound(1), bound(2)];
        reconcile_bindings(&list, &mut live, spawn);
        assert_eq!(spawned.get(), 2);
        assert_eq!(live.len(), 2);

        // A second pass must not re-spawn existing bindings.
        reconcile_bindings(&list, &mut live, spawn);
        assert_eq!(spawned.get(), 2);

        // Removing a connector drops its task.
        reconcile_bindings(&list[..1], &mut live, spawn);
        assert_eq!(live.len(), 1);
        assert!(live.contains_key(&ConnectorId(1)));

        // Clearing the binding drops it too, even though the connector stays.
        let mut unbound = list[0].clone();
        unbound.style.bind = None;
        reconcile_bindings(&[unbound], &mut live, spawn);
        assert!(live.is_empty());
    }

    #[test]
    fn config_round_trips() {
        let c = Connector {
            id: ConnectorId(3),
            points: vec![
                ConnectorAnchor::Widget {
                    id: WidgetId(1),
                    side: Side::Right,
                    t: 0.25,
                },
                ConnectorAnchor::Free { x: 40.0, y: 90.0 },
            ],
            style: ConnectorStyle {
                color: Some(Hsla::default()),
                width: 2.0,
                dashed: true,
                shape: LineShape::Curved,
                arrow: ArrowEnds::Both,
                label: "flow".into(),
                on_top: true,
                bind: Some(ConnectorBinding {
                    component: "sat.pump.on".into(),
                    element: 0,
                    threshold: 0.5,
                    on_color: None,
                }),
            },
        };
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(serde_json::from_str::<Connector>(&s).unwrap(), c);
    }

    #[test]
    fn a_connector_written_before_style_existed_still_loads() {
        let blob = r#"{"id":1,"points":[{"Free":{"x":0.0,"y":0.0}},{"Free":{"x":1.0,"y":1.0}}]}"#;
        let c: Connector = serde_json::from_str(blob).unwrap();
        assert_eq!(c.points.len(), 2);
        assert_eq!(c.style.shape, LineShape::Orthogonal);
        assert!(!c.style.on_top);
    }
}
