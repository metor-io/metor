//! The System Graph tile: a read-only node-and-wire view of a running FSW's
//! wiring topology.
//!
//! Topology comes from the [`WiringStore`](crate::wiring::WiringStore) — the
//! latest [`WiringManifest`](metor_proto_wkt::WiringManifest) the control
//! system broadcast — so the tile always reflects the live target. It never
//! edits anything: nodes (systems, slots, the coordinator, collapsed-scope
//! groups) are laid out deterministically by [`layout`], wires are drawn with
//! the shared [`graph_canvas`](crate::graph_canvas) primitives, and the only
//! state the tile owns is view state (pan, manual node positions, collapsed
//! scopes), persisted through [`SystemGraphConfig`].

pub mod config;
pub mod inspector_rows;
pub mod layout;

#[cfg(test)]
mod tests;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use gpui::{
    App, Axis, Bounds, Context, FocusHandle, Focusable, MouseButton, Pixels, Point, Render,
    SharedString, Window, canvas, div, point, prelude::*, px,
};
use metor_db::DB;
use metor_fsw_2::ir::{EdgeKind, Wiring};

pub use config::SystemGraphConfig;

use crate::graph_canvas::{RoutePoints, hit_test_edges, paint_grid, paint_route};
use crate::graph_layout::Direction;
use crate::theme::theme;
use crate::tiles::PaneItem;
use crate::views::system_graph::config::Viewport;
use crate::views::system_graph::layout::{
    GraphEdge, GraphNode, GraphNodeKind, NODE_WIDTH, card_size, layout,
};

const HEADER_HEIGHT: f32 = 24.0;

struct NodeDrag {
    id: SharedString,
    pointer_origin: Point<Pixels>,
    node_origin: (f32, f32),
    moved: bool,
}

struct PanDrag {
    pointer_origin: Point<Pixels>,
    viewport_origin: Viewport,
}

/// A node's fully-resolved render geometry for one frame.
struct NodeView {
    node: GraphNode,
    graph_pos: (f32, f32),
    height: f32,
}

pub struct SystemGraphPanel {
    focus_handle: FocusHandle,
    viewport: Viewport,
    overrides: HashMap<SharedString, (f32, f32)>,
    collapsed: BTreeSet<String>,
    direction: Direction,
    selection: Option<SharedString>,
    selected_edge: Option<GraphEdge>,
    node_drag: Option<NodeDrag>,
    pan_drag: Option<PanDrag>,
    canvas_origin: Option<Point<Pixels>>,
}

impl SystemGraphPanel {
    pub fn new(_db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        Self::build(SystemGraphConfig::default(), cx)
    }

    pub fn from_config(cfg: SystemGraphConfig, _db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        Self::build(cfg, cx)
    }

    fn build(cfg: SystemGraphConfig, cx: &mut Context<Self>) -> Self {
        // Repaint whenever the manifest store folds a new topology.
        if let Some(store) = crate::wiring::try_global(cx) {
            cx.observe(&store, |_, _, cx| cx.notify()).detach();
        }
        Self {
            focus_handle: cx.focus_handle(),
            viewport: cfg.viewport.clone(),
            overrides: cfg.overrides_map(),
            collapsed: cfg.collapsed_set(),
            direction: cfg.direction,
            selection: None,
            selected_edge: None,
            node_drag: None,
            pan_drag: None,
            canvas_origin: None,
        }
    }

    pub fn direction(&self) -> Direction {
        self.direction
    }

    pub fn set_direction(&mut self, direction: Direction, cx: &mut Context<Self>) {
        if self.direction != direction {
            self.direction = direction;
            cx.notify();
        }
    }

    /// The gpui paint axis matching the current flow direction.
    fn flow_axis(&self) -> Axis {
        match self.direction {
            Direction::LeftRight => Axis::Horizontal,
            Direction::TopBottom => Axis::Vertical,
        }
    }

    fn wiring(&self, cx: &App) -> Option<Wiring> {
        crate::wiring::try_global(cx)?
            .read(cx)
            .state()
            .wiring()
            .cloned()
    }

    fn store_error(&self, cx: &App) -> Option<SharedString> {
        crate::wiring::try_global(cx)?
            .read(cx)
            .state()
            .error()
            .cloned()
    }

    fn graph_pos(&self, node: &GraphNode) -> (f32, f32) {
        self.overrides.get(&node.id).copied().unwrap_or(node.pos)
    }

    fn toggle_collapsed(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.collapsed.remove(path) {
            self.collapsed.insert(path.to_string());
        }
        self.selection = None;
        self.selected_edge = None;
        cx.notify();
    }

    fn relayout(&mut self, cx: &mut Context<Self>) {
        self.overrides.clear();
        cx.notify();
    }

    fn open_node_inspector(
        &self,
        id: SharedString,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let proxy = cx.new(|_| inspector_rows::SelectedGraphNode { id });
        window.dispatch_action(
            Box::new(crate::inspector::InspectEntity {
                entity: proxy.into_any(),
                position,
            }),
            cx,
        );
    }
}

impl Focusable for SystemGraphPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SystemGraphPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);

        let Some(wiring) = self.wiring(cx) else {
            let msg = self
                .store_error(cx)
                .unwrap_or_else(|| "Waiting for wiring manifest…".into());
            return div()
                .track_focus(&self.focus_handle)
                .size_full()
                .bg(theme.bg_primary)
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.text_secondary)
                .text_size(px(12.0))
                .child(msg);
        };

        let graph = layout(&wiring, &self.collapsed, self.direction);
        let viewport = self.viewport.clone();
        let axis = self.flow_axis();

        // Resolve each node's graph-space and screen geometry once.
        let mut views: Vec<NodeView> = Vec::with_capacity(graph.nodes.len());
        let mut geom: HashMap<SharedString, (f32, f32, f32)> = HashMap::new();
        for node in &graph.nodes {
            let gp = self.graph_pos(node);
            let h = card_size(node.kind).1;
            geom.insert(node.id.clone(), (gp.0, gp.1, h));
            views.push(NodeView {
                node: node.clone(),
                graph_pos: gp,
                height: h,
            });
        }

        // Edge routes in canvas-local coordinates. The engine's fanned
        // anchors and waypoints apply as long as both endpoints sit where the
        // layout put them; a manually-dragged endpoint falls back to a plain
        // side-mid wire so it tracks the card under the pointer.
        let selected_edge = self.selected_edge.clone();
        let edge_snapshots: Vec<(GraphEdge, RoutePoints, gpui::Hsla, bool)> = graph
            .edges
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let local = |p: (f32, f32)| point(px(p.0 - viewport.x), px(p.1 - viewport.y));
                let route: RoutePoints = if self.overrides.contains_key(&e.from_node)
                    || self.overrides.contains_key(&e.to_node)
                {
                    let (sx, sy, sh) = geom.get(&e.from_node)?;
                    let (tx, ty, th) = geom.get(&e.to_node)?;
                    let (src, tgt) = match self.direction {
                        Direction::LeftRight => {
                            ((sx + NODE_WIDTH, sy + sh / 2.0), (*tx, ty + th / 2.0))
                        }
                        Direction::TopBottom => (
                            (sx + NODE_WIDTH / 2.0, sy + sh),
                            (tx + NODE_WIDTH / 2.0, *ty),
                        ),
                    };
                    [local(src), local(tgt)].into_iter().collect()
                } else {
                    let r = &graph.routes[i];
                    std::iter::once(local(r.source))
                        .chain(r.waypoints.iter().map(|&w| local(w)))
                        .chain(std::iter::once(local(r.target)))
                        .collect()
                };
                let mut color = match e.kind {
                    EdgeKind::Frame => theme.frame_edge_color(),
                    EdgeKind::Msg => theme.msg_edge_color(),
                };
                if selected_edge.as_ref() == Some(e) {
                    color = theme.line_colors[0];
                }
                Some((e.clone(), route, color, e.delayed))
            })
            .collect();

        let theme_for_paint = theme.clone();
        let edges_for_paint = edge_snapshots.clone();
        let canvas_layer = canvas(
            {
                let weak = cx.entity().downgrade();
                move |bounds: Bounds<Pixels>, _window, cx| {
                    let _ = weak.update(cx, |this, _| this.canvas_origin = Some(bounds.origin));
                    bounds
                }
            },
            move |_, bounds, window, _cx| {
                paint_grid(bounds, theme_for_paint.grid_color, window);
                for (_e, route, color, dashed) in &edges_for_paint {
                    paint_route(bounds.origin, route, axis, *color, *dashed, window);
                }
            },
        )
        .absolute()
        .inset_0();

        let edge_geometry: Arc<Vec<(GraphEdge, RoutePoints)>> = Arc::new(
            edge_snapshots
                .iter()
                .map(|(e, route, _, _)| (e.clone(), route.clone()))
                .collect(),
        );

        let mut root = div()
            .key_context("SystemGraph")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .bg(theme.bg_primary)
            .on_mouse_down(MouseButton::Left, {
                let edges = edge_geometry.clone();
                cx.listener(move |this, ev: &gpui::MouseDownEvent, _w, cx| {
                    let Some(origin) = this.canvas_origin else {
                        return;
                    };
                    let local = ev.position - origin;
                    if let Some(edge) = hit_test_edges(&edges, local, this.flow_axis()) {
                        this.selected_edge = Some(edge);
                        this.selection = None;
                        cx.notify();
                        return;
                    }
                    if this.selection.is_some() || this.selected_edge.is_some() {
                        this.selection = None;
                        this.selected_edge = None;
                        cx.notify();
                    }
                })
            })
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, ev: &gpui::MouseDownEvent, _w, _cx| {
                    this.pan_drag = Some(PanDrag {
                        pointer_origin: ev.position,
                        viewport_origin: this.viewport.clone(),
                    });
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &gpui::MouseMoveEvent, _w, cx| {
                if let Some(pan) = &this.pan_drag {
                    let dx = f32::from(ev.position.x - pan.pointer_origin.x);
                    let dy = f32::from(ev.position.y - pan.pointer_origin.y);
                    this.viewport.x = pan.viewport_origin.x - dx;
                    this.viewport.y = pan.viewport_origin.y - dy;
                    cx.notify();
                    return;
                }
                if let Some(drag) = &mut this.node_drag {
                    let dx = f32::from(ev.position.x - drag.pointer_origin.x);
                    let dy = f32::from(ev.position.y - drag.pointer_origin.y);
                    if dx.abs() > 2.0 || dy.abs() > 2.0 {
                        drag.moved = true;
                    }
                    let id = drag.id.clone();
                    let new_pos = (drag.node_origin.0 + dx, drag.node_origin.1 + dy);
                    this.overrides.insert(id, new_pos);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, ev: &gpui::MouseUpEvent, window, cx| {
                    if let Some(drag) = this.node_drag.take()
                        && !drag.moved
                    {
                        this.open_node_inspector(drag.id, ev.position, window, cx);
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, _ev: &gpui::MouseUpEvent, _w, cx| {
                    if this.pan_drag.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .on_scroll_wheel(
                cx.listener(|this, event: &gpui::ScrollWheelEvent, _window, cx| {
                    let delta = event.delta.pixel_delta(px(20.0));
                    this.viewport.x -= f32::from(delta.x);
                    this.viewport.y -= f32::from(delta.y);
                    cx.notify();
                }),
            );

        // Expanded-scope containers sit behind the wires and cards.
        for container in self.render_scope_containers(&wiring, &views, &theme, cx) {
            root = root.child(container);
        }
        root = root.child(canvas_layer);

        for view in &views {
            root = root.child(self.render_node(view, &wiring, &theme, cx));
        }

        // Selected-edge detail overlay near the wire's midpoint.
        if let Some(sel) = self.selected_edge.as_ref() {
            for (edge, route, _c, _d) in &edge_snapshots {
                if edge != sel {
                    continue;
                }
                // Anchor the label near the route's middle: the central point
                // of an odd polyline, or the midpoint of the central pair.
                let (a, b) = (route[route.len().div_ceil(2) - 1], route[route.len() / 2]);
                let mid_x = (f32::from(a.x) + f32::from(b.x)) / 2.0;
                let mid_y = (f32::from(a.y) + f32::from(b.y)) / 2.0;
                let kind = if edge.kind == EdgeKind::Msg {
                    "msg"
                } else {
                    "frame"
                };
                let delayed = if edge.delayed { " · delayed" } else { "" };
                let label = format!("{} → {} ({kind}{delayed})", edge.from_port, edge.to_port);
                root = root.child(
                    div()
                        .absolute()
                        .left(px(mid_x + 8.0))
                        .top(px(mid_y - 10.0))
                        .px_1p5()
                        .py_0p5()
                        .rounded_md()
                        .bg(theme.bg_elevated)
                        .border_1()
                        .border_color(theme.border_primary)
                        .text_size(px(10.0))
                        .text_color(theme.text_secondary)
                        .child(SharedString::from(label)),
                );
                break;
            }
        }

        root = root.child(self.render_toolbar(&graph, &theme, cx));
        root
    }
}

impl SystemGraphPanel {
    fn render_node(
        &self,
        view: &NodeView,
        wiring: &Wiring,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let NodeView {
            node,
            graph_pos,
            height,
        } = view;
        let id = node.id.clone();
        let screen_x = graph_pos.0 - self.viewport.x;
        let screen_y = graph_pos.1 - self.viewport.y;
        let selected = self.selection.as_ref() == Some(&id);

        let (accent, kind_label) = match node.kind {
            GraphNodeKind::System => (theme.border_primary, "system"),
            GraphNodeKind::Slot => (theme.line_colors[5], "slot"),
            GraphNodeKind::Coordinator => (theme.line_colors[6], "coordinator"),
            GraphNodeKind::ScopeGroup => (theme.line_colors[4], "scope"),
        };
        let border_color = if selected {
            theme.line_colors[0]
        } else {
            accent
        };

        let mut card = div()
            .absolute()
            .left(px(screen_x))
            .top(px(screen_y))
            .w(px(NODE_WIDTH))
            .h(px(*height))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(border_color)
            .rounded_md()
            .overflow_hidden();

        // Header: name (left) + kind tag / badge (right).
        let display_name = match node.kind {
            GraphNodeKind::ScopeGroup => scope_leaf(&id),
            _ => id.clone(),
        };
        card = card.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .px_2()
                .h(px(HEADER_HEIGHT))
                .border_b_1()
                .border_color(theme.border_primary)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(11.0))
                        .text_color(theme.text_primary)
                        .child(display_name),
                )
                .child(
                    div()
                        .text_size(px(9.0))
                        .text_color(theme.text_tertiary)
                        .child(SharedString::new_static(kind_label)),
                ),
        );

        card = card.child(self.node_body(node, wiring, theme));

        // Interaction: a group card toggles collapse; other nodes select +
        // drag, with a non-drag release opening the inspector.
        match node.kind {
            GraphNodeKind::ScopeGroup => {
                let path = id.to_string();
                card.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _ev: &gpui::MouseDownEvent, _w, cx| {
                        this.toggle_collapsed(&path, cx);
                        cx.stop_propagation();
                    }),
                )
            }
            _ => {
                let node_origin = *graph_pos;
                let drag_id = id.clone();
                card.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &gpui::MouseDownEvent, _w, cx| {
                        this.selection = Some(drag_id.clone());
                        this.selected_edge = None;
                        this.node_drag = Some(NodeDrag {
                            id: drag_id.clone(),
                            pointer_origin: ev.position,
                            node_origin,
                            moved: false,
                        });
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
            }
        }
    }

    fn node_body(
        &self,
        node: &GraphNode,
        wiring: &Wiring,
        theme: &crate::theme::Theme,
    ) -> impl IntoElement {
        let body = div()
            .flex()
            .flex_col()
            .gap_0p5()
            .px_2()
            .py_1()
            .text_size(px(10.0))
            .text_color(theme.text_secondary);
        match (node.kind, node.source_index) {
            (GraphNodeKind::System, Some(i)) => {
                let sys = &wiring.systems[i];
                let ty = sys.ty.clone().unwrap_or_else(|| "(default)".into());
                let mut b = body.child(SharedString::from(ty));
                if sys.process {
                    b = b.child(
                        div()
                            .text_size(px(9.0))
                            .text_color(theme.line_colors[6])
                            .child(SharedString::new_static("proc")),
                    );
                }
                b.into_any_element()
            }
            (GraphNodeKind::Slot, Some(i)) => {
                let slot = &wiring.slots[i];
                let occ = slot
                    .allow
                    .iter()
                    .map(|a| a.occupant.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let initial = slot
                    .initial
                    .as_ref()
                    .map(|i| i.occupant.clone())
                    .unwrap_or_else(|| "(empty)".to_string());
                body.child(SharedString::from(format!("allow: {occ}")))
                    .child(SharedString::from(format!("initial: {initial}")))
                    .into_any_element()
            }
            (GraphNodeKind::Coordinator, _) => body
                .child(SharedString::from(format!(
                    "{} Hz",
                    wiring.coordinator.cycle_rate
                )))
                .into_any_element(),
            _ => body
                .child(SharedString::new_static("(collapsed — click to expand)"))
                .into_any_element(),
        }
    }

    fn render_toolbar(
        &self,
        graph: &layout::GraphLayout,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let node_count = graph.nodes.len();
        let edge_count = graph.edges.len();
        div()
            .absolute()
            .left_2()
            .top_2()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                div()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .bg(theme.bg_elevated)
                    .border_1()
                    .border_color(theme.border_primary)
                    .text_size(px(10.0))
                    .text_color(theme.text_secondary)
                    .child(SharedString::from(format!(
                        "{node_count} nodes · {edge_count} edges"
                    ))),
            )
            .child(
                div()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .bg(theme.bg_elevated)
                    .border_1()
                    .border_color(theme.border_primary)
                    .text_size(px(10.0))
                    .text_color(theme.text_primary)
                    .child(SharedString::new_static("Re-layout"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &gpui::MouseDownEvent, _w, cx| {
                            this.relayout(cx);
                            cx.stop_propagation();
                        }),
                    ),
            )
            .child(
                div()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .bg(theme.bg_elevated)
                    .border_1()
                    .border_color(theme.border_primary)
                    .text_size(px(10.0))
                    .text_color(theme.text_primary)
                    .child(SharedString::new_static(match self.direction {
                        Direction::LeftRight => "Flow →",
                        Direction::TopBottom => "Flow ↓",
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _ev: &gpui::MouseDownEvent, _w, cx| {
                            this.set_direction(this.direction.cycle(), cx);
                            cx.stop_propagation();
                        }),
                    ),
            )
    }

    /// One bordered container per expanded scope that has at least one visible
    /// member, sized to the members' bounding box and headed by a collapse
    /// affordance. Nested scopes stack because a child's members are a subset
    /// of its parent's, so the child box sits inside the parent box.
    fn render_scope_containers(
        &self,
        wiring: &Wiring,
        views: &[NodeView],
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        if wiring.scopes.is_empty() {
            return Vec::new();
        }
        // Accumulate each expanded scope's member bounding box in screen space.
        let mut boxes: HashMap<usize, (f32, f32, f32, f32)> = HashMap::new();
        for view in views {
            let scope = match (view.node.kind, view.node.source_index) {
                (GraphNodeKind::System, Some(i)) => wiring.systems[i].scope,
                (GraphNodeKind::Slot, Some(i)) => wiring.slots[i].scope,
                _ => continue,
            };
            let x0 = view.graph_pos.0 - self.viewport.x;
            let y0 = view.graph_pos.1 - self.viewport.y;
            let x1 = x0 + NODE_WIDTH;
            let y1 = y0 + view.height;
            let mut cur = scope;
            while let Some(idx) = cur {
                let Some(spec) = wiring.scopes.get(idx) else {
                    break;
                };
                let entry = boxes.entry(idx).or_insert((x0, y0, x1, y1));
                entry.0 = entry.0.min(x0);
                entry.1 = entry.1.min(y0);
                entry.2 = entry.2.max(x1);
                entry.3 = entry.3.max(y1);
                cur = spec.parent;
            }
        }

        const PAD: f32 = 16.0;
        let mut out = Vec::new();
        for (idx, (x0, y0, x1, y1)) in boxes {
            let path = wiring.scopes[idx].path.clone();
            let left = x0 - PAD;
            let top = y0 - PAD - HEADER_HEIGHT;
            let width = (x1 - x0) + PAD * 2.0;
            let height = (y1 - y0) + PAD * 2.0 + HEADER_HEIGHT;
            let collapse_path = path.clone();
            out.push(
                div()
                    .absolute()
                    .left(px(left))
                    .top(px(top))
                    .w(px(width))
                    .h(px(height))
                    .rounded_md()
                    .border_1()
                    .border_color(theme.line_colors[4])
                    .child(
                        div()
                            .absolute()
                            .left_1()
                            .top_1()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .text_size(px(9.0))
                            .text_color(theme.line_colors[4])
                            .child(scope_leaf(&path))
                            .child(
                                div()
                                    .text_color(theme.text_tertiary)
                                    .child(SharedString::new_static("[collapse]"))
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(
                                            move |this, _ev: &gpui::MouseDownEvent, _w, cx| {
                                                this.toggle_collapsed(&collapse_path, cx);
                                                cx.stop_propagation();
                                            },
                                        ),
                                    ),
                            ),
                    )
                    .into_any_element(),
            );
        }
        out
    }
}

/// The last dotted segment of a scope path, for the group card's header.
fn scope_leaf(path: &str) -> SharedString {
    match path.rsplit_once('.') {
        Some((_, leaf)) => SharedString::from(leaf.to_string()),
        None => SharedString::from(path.to_string()),
    }
}

impl PaneItem for SystemGraphPanel {
    type Config = SystemGraphConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        "System Graph".into()
    }

    fn serialization_key() -> &'static str {
        "system_graph"
    }

    fn to_config(&self, _cx: &App) -> SystemGraphConfig {
        SystemGraphConfig::from_state(
            self.viewport.clone(),
            &self.overrides,
            &self.collapsed,
            self.direction,
        )
    }
}
