//! `NodeEditor`: the visual canvas pane that hosts a [`NodeGraph`]. Renders
//! nodes as positioned boxes, edges as bezier paths, and wires drag-to-move
//! / drag-to-connect / delete interactions back into the graph. Mutations
//! are followed by a 200ms-debounced `rebuild` so unrelated subtrees stay
//! live across keystrokes.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, Hsla, IntoElement, MouseButton,
    PathBuilder, Pixels, Point, Render, SharedString, Task, WeakEntity, Window, canvas, div,
    point, prelude::*, px,
};
use metor_db::DB;

use crate::node_editor::config::{NodeEditorConfig, Viewport as ConfigViewport};
use crate::node_editor::graph::{
    BuildState, EdgeEntry, FlowId, NodeEntry, NodeGraph, Position,
};
use crate::node_editor::registry::{Arity, OpDescriptor, SocketKind, descriptor_for};
use crate::node_editor::validate::{EdgeColor, EdgeVerdict, edge_color, validate_connection};
use crate::theme::theme;
use crate::tiles::PaneItem;

const NODE_WIDTH: f32 = 168.0;
const HEADER_HEIGHT: f32 = 26.0;
const SOCKET_ROW_HEIGHT: f32 = 20.0;
const SOCKET_DOT_SIZE: f32 = 10.0;
const NODE_VPAD: f32 = 6.0;

/// Track the most recently focused `NodeEditor` so palette commands can
/// route "Add Node" to the right place.
#[derive(Default)]
pub struct ActiveNodeEditor(pub Option<WeakEntity<NodeEditor>>);

impl gpui::Global for ActiveNodeEditor {}

impl ActiveNodeEditor {
    pub fn init(cx: &mut App) {
        cx.set_global(ActiveNodeEditor::default());
    }

    pub fn set(editor: WeakEntity<NodeEditor>, cx: &mut App) {
        cx.global_mut::<ActiveNodeEditor>().0 = Some(editor);
    }

    pub fn get(cx: &App) -> Option<WeakEntity<NodeEditor>> {
        cx.global::<ActiveNodeEditor>().0.clone()
    }
}

#[derive(Clone, PartialEq, gpui::Action)]
#[action(namespace = node_editor)]
pub struct DeleteSelected;

struct NodeDrag {
    flow_id: FlowId,
    pointer_origin: Point<Pixels>,
    node_origin: Position,
}

struct EdgeDraft {
    source: FlowId,
    /// Current mouse position in canvas-local coordinates.
    pointer: Point<Pixels>,
}

struct PanDrag {
    pointer_origin: Point<Pixels>,
    viewport_origin: ConfigViewport,
}

pub struct NodeEditor {
    graph: Entity<NodeGraph>,
    db: Arc<DB>,
    focus_handle: FocusHandle,
    selection: Option<FlowId>,
    selected_edge: Option<EdgeEntry>,
    viewport: ConfigViewport,
    node_drag: Option<NodeDrag>,
    edge_draft: Option<EdgeDraft>,
    pan_drag: Option<PanDrag>,
    canvas_origin: Option<Point<Pixels>>,
    rebuild_task: Option<Task<()>>,
    next_node_seq: u64,
}

impl NodeEditor {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let owner = cx.entity_id().as_u64();
        let graph = cx.new(|_| NodeGraph::new(owner));
        let focus_handle = cx.focus_handle();
        Self {
            graph,
            db,
            focus_handle,
            selection: None,
            selected_edge: None,
            viewport: ConfigViewport::default(),
            node_drag: None,
            edge_draft: None,
            pan_drag: None,
            canvas_origin: None,
            rebuild_task: None,
            next_node_seq: 0,
        }
    }

    pub fn from_config(cfg: NodeEditorConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let viewport = cfg.viewport.clone();
        let next_node_seq = cfg.nodes.len() as u64;
        let graph = cfg.into_graph();
        let owner = cx.entity_id().as_u64();
        // The hydrated graph carries its serialized owner_id, but every
        // process gets a fresh EntityId, so override with our actual id.
        let mut graph = graph;
        graph.owner_id = owner;
        let graph = cx.new(|_| graph);
        let focus_handle = cx.focus_handle();
        let mut this = Self {
            graph,
            db,
            focus_handle,
            selection: None,
            selected_edge: None,
            viewport,
            node_drag: None,
            edge_draft: None,
            pan_drag: None,
            canvas_origin: None,
            rebuild_task: None,
            next_node_seq,
        };
        this.schedule_rebuild(cx);
        this
    }

    fn next_flow_id(&mut self, kind: &str) -> FlowId {
        let n = self.next_node_seq;
        self.next_node_seq += 1;
        SharedString::from(format!("{kind}-{n}"))
    }

    /// Spawn a new node at canvas-local pixel position `screen_pos` (graph
    /// origin assumed at (0, 0); pan/zoom not yet wired). Used by the palette
    /// provider — the descriptor decides label and default args.
    /// Read-only accessor for the underlying graph entity. Used by
    /// inspector_rows to walk the spec without going through `to_config`.
    pub fn graph_entity(&self) -> &Entity<NodeGraph> {
        &self.graph
    }

    /// Read-only DB handle for inspector rows that need to enumerate
    /// components (e.g. `FromDb`'s picker).
    pub fn db(&self) -> &Arc<DB> {
        &self.db
    }

    /// Schedule a rebuild from outside the pane (e.g. an inspector arg edit).
    pub fn bump_rebuild(&mut self, cx: &mut Context<Self>) {
        self.schedule_rebuild(cx);
    }

    pub fn add_node(&mut self, descriptor: &OpDescriptor, cx: &mut Context<Self>) {
        let spec = (descriptor.default_spec)();
        let (x, y) = self.next_node_position();
        let id = self.next_flow_id(descriptor.label);
        self.graph.update(cx, |g, _| {
            g.insert_node(id.clone(), spec, Position { x, y });
        });
        self.selection = Some(id);
        self.schedule_rebuild(cx);
        cx.notify();
    }

    fn next_node_position(&self) -> (f32, f32) {
        // Stagger near the upper-left so it's visible on a fresh canvas; nudge
        // diagonally so successive adds don't stack.
        let n = self.next_node_seq as f32;
        (24.0 + (n % 6.0) * 24.0, 24.0 + n * 12.0)
    }

    fn schedule_rebuild(&mut self, cx: &mut Context<Self>) {
        // Drop any pending task to cancel its timer.
        self.rebuild_task.take();
        let graph = self.graph.clone();
        let db = self.db.clone();
        self.rebuild_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let _ = cx.update(|cx| {
                graph.update(cx, |g, cx_inner| g.rebuild(&db, cx_inner));
            });
            let _ = this.update(cx, |_, cx| cx.notify());
        }));
    }

    fn on_delete(&mut self, _: &DeleteSelected, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(edge) = self.selected_edge.take() {
            self.graph.update(cx, |g, _| g.remove_edge(&edge));
            self.schedule_rebuild(cx);
            cx.notify();
            return;
        }
        if let Some(id) = self.selection.take() {
            self.graph.update(cx, |g, _| g.remove_node(&id));
            self.schedule_rebuild(cx);
            cx.notify();
        }
    }
}

impl Drop for NodeEditor {
    fn drop(&mut self) {
        // Best-effort: we don't have cx in Drop, so coordinator cleanup
        // happens lazily when the next editor's rebuild runs (DynamicRegistry
        // entries with no surviving owner get dropped). For an explicit
        // release, call `release` from a `cx.on_release`-style hook.
    }
}

impl Focusable for NodeEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// -----------------------------------------------------------------------------
// Geometry helpers
// -----------------------------------------------------------------------------

/// Number of input sockets shown on `entry`. For variadic ops we render a
/// single "+" affordance plus one row per existing edge.
fn input_count(entry: &NodeEntry, edges: &[EdgeEntry], flow_id: &FlowId) -> usize {
    match descriptor_for(&entry.spec).inputs {
        Arity::Exact(slots) => slots.len(),
        Arity::Variadic { min, .. } => {
            let connected = edges.iter().filter(|e| &e.target == flow_id).count();
            connected.max(min).max(1)
        }
    }
}

fn node_height(input_count: usize) -> f32 {
    let inputs = input_count.max(1);
    HEADER_HEIGHT + NODE_VPAD * 2.0 + (inputs as f32) * SOCKET_ROW_HEIGHT
}

fn input_socket_local_y(socket_index: usize) -> f32 {
    HEADER_HEIGHT + NODE_VPAD + (socket_index as f32 + 0.5) * SOCKET_ROW_HEIGHT
}

fn output_socket_local_y(input_count: usize) -> f32 {
    node_height(input_count) / 2.0
}

/// Color a socket dot by its declared kind.
fn socket_dot_color(kind: SocketKind, theme: &crate::theme::Theme) -> Hsla {
    match kind {
        SocketKind::Clock => theme.line_colors[1],     // cool blue family
        SocketKind::F64Scalar => theme.line_colors[0], // accent orange
        SocketKind::Any => theme.text_secondary,
    }
}

fn edge_color_to_hsla(c: EdgeColor, theme: &crate::theme::Theme) -> Hsla {
    match c {
        EdgeColor::SharedClock(idx) => theme.line_colors[idx % 8],
        EdgeColor::Neutral => theme.border_primary,
        EdgeColor::Error => Hsla {
            h: 0.0,
            s: 0.7,
            l: 0.55,
            a: 1.0,
        },
        EdgeColor::Pending => Hsla {
            a: 0.4,
            ..theme.text_tertiary
        },
    }
}

/// Status pill text for the node header.
fn status_pill(entry: &NodeEntry) -> (&'static str, bool) {
    match &entry.build {
        BuildState::Built(_) => ("●", false),
        BuildState::Pending => ("…", false),
        BuildState::Error(_) => ("!", true),
    }
}

// -----------------------------------------------------------------------------
// Render
// -----------------------------------------------------------------------------

impl Render for NodeEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let graph_ref = self.graph.read(cx);
        let viewport = self.viewport.clone();
        let selected_edge = self.selected_edge.clone();

        // Per-node screen position = graph position - viewport offset.
        let mut node_snapshots: Vec<(FlowId, Position, Position, &'static OpDescriptor, usize, (&'static str, bool))> =
            Vec::with_capacity(graph_ref.nodes.len());
        for (id, entry) in &graph_ref.nodes {
            let descriptor = descriptor_for(&entry.spec);
            let inputs = input_count(entry, &graph_ref.edges, id);
            let pill = status_pill(entry);
            let screen = Position {
                x: entry.position.x - viewport.x,
                y: entry.position.y - viewport.y,
            };
            node_snapshots.push((id.clone(), entry.position.clone(), screen, descriptor, inputs, pill));
        }

        // Pre-compute edges in canvas-local coordinates. We retain the
        // EdgeEntry alongside the geometry so the click handler can hit-test
        // and select the underlying graph edge.
        let edge_snapshots: Vec<(EdgeEntry, Point<Pixels>, Point<Pixels>, Hsla)> = graph_ref
            .edges
            .iter()
            .filter_map(|edge| {
                let source = graph_ref.nodes.get(&edge.source)?;
                let target = graph_ref.nodes.get(&edge.target)?;
                let s_inputs = input_count(source, &graph_ref.edges, &edge.source);
                let src_x = source.position.x - viewport.x + NODE_WIDTH;
                let src_y = source.position.y - viewport.y + output_socket_local_y(s_inputs);
                let tgt_x = target.position.x - viewport.x;
                let tgt_y = target.position.y - viewport.y + input_socket_local_y(edge.target_socket);
                let mut color = edge_color_to_hsla(edge_color(graph_ref, edge), &theme);
                if Some(edge) == selected_edge.as_ref() {
                    color = theme.line_colors[0];
                }
                Some((
                    edge.clone(),
                    point(px(src_x), px(src_y)),
                    point(px(tgt_x), px(tgt_y)),
                    color,
                ))
            })
            .collect();

        let edge_draft = self.edge_draft.as_ref().and_then(|draft| {
            let source = graph_ref.nodes.get(&draft.source)?;
            let s_inputs = input_count(source, &graph_ref.edges, &draft.source);
            let src_x = source.position.x - viewport.x + NODE_WIDTH;
            let src_y = source.position.y - viewport.y + output_socket_local_y(s_inputs);
            Some((point(px(src_x), px(src_y)), draft.pointer, theme.text_secondary))
        });

        let _ = graph_ref;

        let theme_for_paint = theme.clone();
        let edge_snapshots_for_paint = edge_snapshots.clone();

        let canvas_layer = canvas(
            {
                let weak = cx.entity().downgrade();
                move |bounds: Bounds<Pixels>, _window, cx| {
                    let _ = weak.update(cx, |this, _| {
                        this.canvas_origin = Some(bounds.origin);
                    });
                    bounds
                }
            },
            move |_, bounds, window, _cx| {
                paint_grid(bounds, &theme_for_paint, window);
                for (_edge, src, tgt, color) in &edge_snapshots_for_paint {
                    paint_bezier(bounds.origin, *src, *tgt, *color, false, window);
                }
                if let Some((src, tgt, color)) = edge_draft {
                    paint_bezier(bounds.origin, src, tgt, color, true, window);
                }
            },
        )
        .absolute()
        .inset_0();

        let edge_geometry: Arc<Vec<(EdgeEntry, Point<Pixels>, Point<Pixels>)>> = Arc::new(
            edge_snapshots
                .iter()
                .map(|(edge, s, t, _)| (edge.clone(), *s, *t))
                .collect(),
        );

        let mut root = div()
            .key_context("NodeEditor")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_delete))
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
                    if let Some(edge) = hit_test_edges(&edges, local) {
                        this.selected_edge = Some(edge);
                        this.selection = None;
                        this.edge_draft = None;
                        cx.notify();
                        return;
                    }
                    // Click on empty canvas clears selection and any in-progress
                    // edge draft.
                    if this.selection.is_some()
                        || this.selected_edge.is_some()
                        || this.edge_draft.is_some()
                    {
                        this.selection = None;
                        this.selected_edge = None;
                        this.edge_draft = None;
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
            .on_mouse_move(cx.listener(
                |this, ev: &gpui::MouseMoveEvent, _w, cx| {
                    let Some(origin) = this.canvas_origin else {
                        return;
                    };
                    let local = ev.position - origin;
                    if let Some(pan) = &this.pan_drag {
                        let dx = f32::from(ev.position.x - pan.pointer_origin.x);
                        let dy = f32::from(ev.position.y - pan.pointer_origin.y);
                        // Drag right → world moves right under cursor → viewport.x decreases.
                        this.viewport.x = pan.viewport_origin.x - dx;
                        this.viewport.y = pan.viewport_origin.y - dy;
                        cx.notify();
                        return;
                    }
                    if let Some(drag) = &this.node_drag {
                        let dx = f32::from(ev.position.x - drag.pointer_origin.x);
                        let dy = f32::from(ev.position.y - drag.pointer_origin.y);
                        let target_id = drag.flow_id.clone();
                        let new_pos = Position {
                            x: drag.node_origin.x + dx,
                            y: drag.node_origin.y + dy,
                        };
                        this.graph.update(cx, |g, _| {
                            if let Some(entry) = g.nodes.get_mut(&target_id) {
                                entry.position = new_pos;
                            }
                        });
                        cx.notify();
                    } else if let Some(draft) = &mut this.edge_draft {
                        draft.pointer = local;
                        cx.notify();
                    }
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _ev: &gpui::MouseUpEvent, _w, cx| {
                    let was_dragging_node = this.node_drag.take().is_some();
                    let cancelled_draft = this.edge_draft.take().is_some();
                    if was_dragging_node {
                        // Position-only changes don't require a rebuild — but
                        // re-running rebuild is cheap thanks to idempotency,
                        // and Mean's parent order is position-derived so vertical
                        // drags can change its hash.
                        this.schedule_rebuild(cx);
                    }
                    if cancelled_draft {
                        cx.notify();
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
            .child(canvas_layer);

        for (flow_id, graph_pos, screen_pos, descriptor, inputs, pill) in node_snapshots {
            root = root.child(self.render_node(
                flow_id, graph_pos, screen_pos, descriptor, inputs, pill, &theme, cx,
            ));
        }

        root
    }
}

impl NodeEditor {
    #[allow(clippy::too_many_arguments)]
    fn render_node(
        &self,
        flow_id: FlowId,
        graph_pos: Position,
        screen_pos: Position,
        descriptor: &'static OpDescriptor,
        inputs: usize,
        (pill_text, pill_error): (&'static str, bool),
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let height = node_height(inputs);
        let selected = self.selection.as_ref() == Some(&flow_id);

        let border_color = if selected {
            theme.line_colors[0]
        } else {
            theme.border_primary
        };
        let pill_color = if pill_error {
            Hsla { h: 0.0, s: 0.7, l: 0.55, a: 1.0 }
        } else {
            theme.text_secondary
        };

        let mut body = div()
            .absolute()
            .left(px(screen_pos.x))
            .top(px(screen_pos.y))
            .w(px(NODE_WIDTH))
            .h(px(height))
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(border_color)
            .rounded_md()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener({
                    let id = flow_id.clone();
                    let pos = graph_pos.clone();
                    move |this, ev: &gpui::MouseDownEvent, _w, cx| {
                        this.selection = Some(id.clone());
                        this.selected_edge = None;
                        this.node_drag = Some(NodeDrag {
                            flow_id: id.clone(),
                            pointer_origin: ev.position,
                            node_origin: pos.clone(),
                        });
                        cx.stop_propagation();
                        cx.notify();
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener({
                    let id = flow_id.clone();
                    move |this, ev: &gpui::MouseDownEvent, window, cx| {
                        this.selection = Some(id.clone());
                        let editor_entity = cx.entity().clone();
                        let graph_entity = this.graph.clone();
                        let id_for_proxy = id.clone();
                        let proxy = cx.new(|_| crate::node_editor::inspector_rows::SelectedNodeProxy {
                            editor: editor_entity,
                            graph: graph_entity,
                            flow_id: id_for_proxy,
                        });
                        window.dispatch_action(
                            Box::new(crate::inspector::InspectEntity {
                                entity: proxy.into_any(),
                                position: ev.position,
                            }),
                            cx,
                        );
                        cx.stop_propagation();
                        cx.notify();
                    }
                }),
            );

        // Header bar: label + status pill.
        body = body.child(
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
                        .text_size(px(11.0))
                        .text_color(theme.text_primary)
                        .child(SharedString::from(descriptor.label)),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(pill_color)
                        .child(SharedString::from(pill_text)),
                ),
        );

        // Input sockets (left edge).
        let arity = descriptor.inputs;
        for i in 0..inputs {
            let socket_kind = arity.socket_at(i).unwrap_or(SocketKind::Any);
            let dot_color = socket_dot_color(socket_kind, theme);
            let socket_y = input_socket_local_y(i) - SOCKET_DOT_SIZE / 2.0;
            body = body.child(
                div()
                    .absolute()
                    .left(px(-(SOCKET_DOT_SIZE / 2.0)))
                    .top(px(socket_y))
                    .w(px(SOCKET_DOT_SIZE))
                    .h(px(SOCKET_DOT_SIZE))
                    .rounded_full()
                    .bg(dot_color)
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener({
                            let target_id = flow_id.clone();
                            move |this, _ev: &gpui::MouseUpEvent, _w, cx| {
                                let Some(draft) = this.edge_draft.take() else {
                                    return;
                                };
                                let edge = EdgeEntry {
                                    source: draft.source,
                                    target: target_id.clone(),
                                    target_socket: i,
                                };
                                let verdict = {
                                    let g = this.graph.read(cx);
                                    validate_connection(g, &edge)
                                };
                                if matches!(verdict, EdgeVerdict::Ok) {
                                    this.graph.update(cx, |g, _| g.add_edge(edge));
                                    this.schedule_rebuild(cx);
                                }
                                cx.stop_propagation();
                                cx.notify();
                            }
                        }),
                    ),
            );
        }

        // Output socket (right edge).
        let out_y = output_socket_local_y(inputs) - SOCKET_DOT_SIZE / 2.0;
        let out_color = socket_dot_color(descriptor.output, theme);
        body = body.child(
            div()
                .absolute()
                .left(px(NODE_WIDTH - SOCKET_DOT_SIZE / 2.0))
                .top(px(out_y))
                .w(px(SOCKET_DOT_SIZE))
                .h(px(SOCKET_DOT_SIZE))
                .rounded_full()
                .bg(out_color)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener({
                        let source_id = flow_id.clone();
                        move |this, ev: &gpui::MouseDownEvent, _w, cx| {
                            let local = if let Some(origin) = this.canvas_origin {
                                ev.position - origin
                            } else {
                                ev.position
                            };
                            this.edge_draft = Some(EdgeDraft {
                                source: source_id.clone(),
                                pointer: local,
                            });
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }),
                ),
        );

        body
    }
}

// -----------------------------------------------------------------------------
// PaneItem
// -----------------------------------------------------------------------------

impl PaneItem for NodeEditor {
    type Config = NodeEditorConfig;

    fn tab_title(&self, _cx: &App) -> SharedString {
        "Node Editor".into()
    }

    fn serialization_key() -> &'static str {
        "node_editor"
    }

    fn to_config(&self, cx: &App) -> NodeEditorConfig {
        NodeEditorConfig::from_graph(self.graph.read(cx), self.viewport.clone())
    }
}

// -----------------------------------------------------------------------------
// Painting
// -----------------------------------------------------------------------------

fn paint_grid(bounds: Bounds<Pixels>, theme: &crate::theme::Theme, window: &mut Window) {
    let step = 24.0_f32;
    let mut path = PathBuilder::stroke(px(0.5));
    let origin_x = f32::from(bounds.origin.x);
    let origin_y = f32::from(bounds.origin.y);
    let end_x = origin_x + f32::from(bounds.size.width);
    let end_y = origin_y + f32::from(bounds.size.height);
    let mut x = origin_x;
    while x < end_x {
        path.move_to(point(px(x), bounds.origin.y));
        path.line_to(point(px(x), bounds.origin.y + bounds.size.height));
        x += step;
    }
    let mut y = origin_y;
    while y < end_y {
        path.move_to(point(bounds.origin.x, px(y)));
        path.line_to(point(bounds.origin.x + bounds.size.width, px(y)));
        y += step;
    }
    if let Ok(p) = path.build() {
        window.paint_path(p, theme.grid_color);
    }
}

/// Find the first edge whose bezier passes within `HIT_RADIUS` pixels of
/// `pointer` (pointer is in canvas-local coordinates). Samples each curve at
/// 16 points and uses point-to-segment distance.
fn hit_test_edges(
    edges: &[(EdgeEntry, Point<Pixels>, Point<Pixels>)],
    pointer: Point<Pixels>,
) -> Option<EdgeEntry> {
    const SAMPLES: usize = 16;
    const HIT_RADIUS: f32 = 6.0;
    let px_pointer = (f32::from(pointer.x), f32::from(pointer.y));
    let mut best: Option<(f32, EdgeEntry)> = None;
    for (edge, s, t) in edges {
        let sx = f32::from(s.x);
        let sy = f32::from(s.y);
        let tx = f32::from(t.x);
        let ty = f32::from(t.y);
        let dx = (tx - sx).abs().max(40.0) * 0.5;
        let c1 = (sx + dx, sy);
        let c2 = (tx - dx, ty);
        let mut prev = (sx, sy);
        let mut min_dist = f32::INFINITY;
        for i in 1..=SAMPLES {
            let t = i as f32 / SAMPLES as f32;
            let p = cubic_bezier_point(t, (sx, sy), c1, c2, (tx, ty));
            min_dist = min_dist.min(point_segment_distance(px_pointer, prev, p));
            prev = p;
        }
        if min_dist <= HIT_RADIUS {
            match &best {
                Some((d, _)) if *d <= min_dist => {}
                _ => best = Some((min_dist, edge.clone())),
            }
        }
    }
    best.map(|(_, e)| e)
}

fn cubic_bezier_point(
    t: f32,
    p0: (f32, f32),
    p1: (f32, f32),
    p2: (f32, f32),
    p3: (f32, f32),
) -> (f32, f32) {
    let mt = 1.0 - t;
    let b0 = mt * mt * mt;
    let b1 = 3.0 * mt * mt * t;
    let b2 = 3.0 * mt * t * t;
    let b3 = t * t * t;
    (
        b0 * p0.0 + b1 * p1.0 + b2 * p2.0 + b3 * p3.0,
        b0 * p0.1 + b1 * p1.1 + b2 * p2.1 + b3 * p3.1,
    )
}

fn point_segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let ax = b.0 - a.0;
    let ay = b.1 - a.1;
    let len2 = ax * ax + ay * ay;
    if len2 < 1e-6 {
        let dx = p.0 - a.0;
        let dy = p.1 - a.1;
        return (dx * dx + dy * dy).sqrt();
    }
    let t = ((p.0 - a.0) * ax + (p.1 - a.1) * ay) / len2;
    let t = t.clamp(0.0, 1.0);
    let cx = a.0 + t * ax;
    let cy = a.1 + t * ay;
    let dx = p.0 - cx;
    let dy = p.1 - cy;
    (dx * dx + dy * dy).sqrt()
}

fn paint_bezier(
    canvas_origin: Point<Pixels>,
    source: Point<Pixels>,
    target: Point<Pixels>,
    color: Hsla,
    dashed: bool,
    window: &mut Window,
) {
    // `source` and `target` are in canvas-local coordinates; shift to window.
    let s = point(canvas_origin.x + source.x, canvas_origin.y + source.y);
    let t = point(canvas_origin.x + target.x, canvas_origin.y + target.y);

    // Smooth horizontal cubic with control points pulled outward.
    let sx = f32::from(s.x);
    let tx = f32::from(t.x);
    let dx = (tx - sx).abs().max(40.0) * 0.5;
    let c1 = point(px(sx + dx), s.y);
    let c2 = point(px(tx - dx), t.y);

    let stroke = if dashed { px(1.0) } else { px(1.5) };
    let mut path = PathBuilder::stroke(stroke);
    path.move_to(s);
    path.cubic_bezier_to(t, c1, c2);
    if let Ok(p) = path.build() {
        window.paint_path(p, color);
    }
}
