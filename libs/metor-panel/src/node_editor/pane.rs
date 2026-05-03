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

use crate::dynamic::DynamicRegistry;
use crate::node_editor::config::{NodeEditorConfig, Viewport as ConfigViewport};
use crate::node_editor::coordinator::GraphCoordinator;
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

pub struct NodeEditor {
    graph: Entity<NodeGraph>,
    db: Arc<DB>,
    focus_handle: FocusHandle,
    selection: Option<FlowId>,
    viewport: ConfigViewport,
    node_drag: Option<NodeDrag>,
    edge_draft: Option<EdgeDraft>,
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
            viewport: ConfigViewport::default(),
            node_drag: None,
            edge_draft: None,
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
            viewport,
            node_drag: None,
            edge_draft: None,
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
        let owner = self.graph.read(cx).owner_id;
        self.rebuild_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let _ = cx.update(|cx| {
                let alive = graph.update(cx, |g, cx_inner| {
                    let registry = cx_inner.global_mut::<DynamicRegistry>();
                    g.rebuild_into(&db, registry)
                });
                GraphCoordinator::submit(owner, alive, cx);
            });
            let _ = this.update(cx, |_, cx| cx.notify());
        }));
    }

    fn on_delete(&mut self, _: &DeleteSelected, _: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.selection.take() else {
            return;
        };
        self.graph.update(cx, |g, _| {
            g.remove_node(&id);
        });
        self.schedule_rebuild(cx);
        cx.notify();
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

        // Snapshot everything the paint closure and per-node renderers need
        // so we can drop the borrow on `graph` before composing children.
        let mut node_snapshots: Vec<(FlowId, Position, &'static OpDescriptor, usize, (&'static str, bool))> =
            Vec::with_capacity(graph_ref.nodes.len());
        for (id, entry) in &graph_ref.nodes {
            let descriptor = descriptor_for(&entry.spec);
            let inputs = input_count(entry, &graph_ref.edges, id);
            let pill = status_pill(entry);
            node_snapshots.push((id.clone(), entry.position.clone(), descriptor, inputs, pill));
        }

        // Pre-compute edges in screen-relative coordinates for the paint pass.
        let edge_snapshots: Vec<(Point<Pixels>, Point<Pixels>, Hsla)> = graph_ref
            .edges
            .iter()
            .filter_map(|edge| {
                let source = graph_ref.nodes.get(&edge.source)?;
                let target = graph_ref.nodes.get(&edge.target)?;
                let s_inputs = input_count(source, &graph_ref.edges, &edge.source);
                let src_x = source.position.x + NODE_WIDTH;
                let src_y = source.position.y + output_socket_local_y(s_inputs);
                let tgt_x = target.position.x;
                let tgt_y = target.position.y + input_socket_local_y(edge.target_socket);
                let color = edge_color_to_hsla(edge_color(graph_ref, edge), &theme);
                Some((
                    point(px(src_x), px(src_y)),
                    point(px(tgt_x), px(tgt_y)),
                    color,
                ))
            })
            .collect();

        let edge_draft = self.edge_draft.as_ref().and_then(|draft| {
            let source = graph_ref.nodes.get(&draft.source)?;
            let s_inputs = input_count(source, &graph_ref.edges, &draft.source);
            let src_x = source.position.x + NODE_WIDTH;
            let src_y = source.position.y + output_socket_local_y(s_inputs);
            Some((point(px(src_x), px(src_y)), draft.pointer, theme.text_secondary))
        });

        let _ = graph_ref;

        let theme_for_paint = theme.clone();

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
                for (src, tgt, color) in &edge_snapshots {
                    paint_bezier(bounds.origin, *src, *tgt, *color, false, window);
                }
                if let Some((src, tgt, color)) = edge_draft {
                    paint_bezier(bounds.origin, src, tgt, color, true, window);
                }
            },
        )
        .absolute()
        .inset_0();

        let mut root = div()
            .key_context("NodeEditor")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_delete))
            .relative()
            .size_full()
            .bg(theme.bg_primary)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev: &gpui::MouseDownEvent, _w, cx| {
                    // Click on empty canvas clears selection and any in-progress
                    // edge draft.
                    if this.selection.is_some() || this.edge_draft.is_some() {
                        this.selection = None;
                        this.edge_draft = None;
                        cx.notify();
                    }
                }),
            )
            .on_mouse_move(cx.listener(
                |this, ev: &gpui::MouseMoveEvent, _w, cx| {
                    let Some(origin) = this.canvas_origin else {
                        return;
                    };
                    let local = ev.position - origin;
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
            .child(canvas_layer);

        for (flow_id, position, descriptor, inputs, pill) in node_snapshots {
            root = root.child(self.render_node(flow_id, position, descriptor, inputs, pill, &theme, cx));
        }

        root
    }
}

impl NodeEditor {
    #[allow(clippy::too_many_arguments)]
    fn render_node(
        &self,
        flow_id: FlowId,
        position: Position,
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
            .left(px(position.x))
            .top(px(position.y))
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
                    let pos = position.clone();
                    move |this, ev: &gpui::MouseDownEvent, _w, cx| {
                        this.selection = Some(id.clone());
                        this.node_drag = Some(NodeDrag {
                            flow_id: id.clone(),
                            pointer_origin: ev.position,
                            node_origin: pos.clone(),
                        });
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
