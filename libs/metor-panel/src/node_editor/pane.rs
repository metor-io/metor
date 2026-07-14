//! `NodeEditor`: the visual canvas pane that hosts a [`NodeGraph`]. Renders
//! nodes as positioned boxes, edges as bezier paths, and wires drag-to-move
//! / drag-to-connect / delete interactions back into the graph. Mutations
//! are followed by a 200ms-debounced `rebuild` so unrelated subtrees stay
//! live across keystrokes.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, Hsla, IntoElement, MouseButton, Pixels,
    Point, Render, SharedString, Task, WeakEntity, Window, canvas, div, point, prelude::*, px,
};
use metor_db::DB;

use crate::graph_canvas::{RoutePoints, hit_test_edges, paint_bezier, paint_grid};
use crate::node_editor::config::{NodeEditorConfig, Viewport as ConfigViewport};
use crate::node_editor::coordinator::GraphCoordinator;
use crate::node_editor::graph::{BuildState, EdgeEntry, FlowId, NodeEntry, NodeGraph, Position};
use crate::node_editor::registry::{Arity, OpDescriptor, SocketKind, descriptor_for};
use crate::node_editor::spec::NodeSpec;
use crate::node_editor::validate::{EdgeColor, EdgeVerdict, edge_color, validate_connection};
use crate::theme::theme;
use crate::tiles::PaneItem;

const NODE_WIDTH: f32 = 168.0;
const HEADER_HEIGHT: f32 = 26.0;
const SOCKET_ROW_HEIGHT: f32 = 20.0;
const SOCKET_DOT_SIZE: f32 = 10.0;

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
    canvas_size: Option<gpui::Size<Pixels>>,
    rebuild_task: Option<Task<()>>,
    next_node_seq: u64,
    /// Per-node inline row lists keyed by `FlowId`. The cached spec lets
    /// us detect when a node's args changed (e.g. after the user edits
    /// freq) so we only call `RowList::set_rows` then — keeping the row
    /// list's edit state intact across unrelated re-renders.
    node_row_lists: HashMap<FlowId, NodeRowEntry>,
}

struct NodeRowEntry {
    spec: NodeSpec,
    list: Entity<crate::inspector::row_list::RowList>,
}

/// Per-node data captured once per render, then handed to `render_node`.
/// Avoids re-borrowing the graph entity inside the per-node loop.
struct NodeRenderSnapshot {
    flow_id: FlowId,
    graph_pos: Position,
    screen_pos: Position,
    descriptor: &'static OpDescriptor,
    inputs: usize,
    args: usize,
    pill: (&'static str, bool),
}

impl NodeEditor {
    pub fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let owner = cx.entity_id().as_u64();
        cx.on_release(move |_, cx| GraphCoordinator::drop_owner(owner, cx))
            .detach();
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
            canvas_size: None,
            rebuild_task: None,
            next_node_seq: 0,
            node_row_lists: HashMap::new(),
        }
    }

    pub fn from_config(cfg: NodeEditorConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let viewport = cfg.viewport.clone();
        let next_node_seq = cfg.nodes.len() as u64;
        let owner = cx.entity_id().as_u64();
        cx.on_release(move |_, cx| GraphCoordinator::drop_owner(owner, cx))
            .detach();
        let graph = cx.new(|_| cfg.into_graph(owner));
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
            canvas_size: None,
            rebuild_task: None,
            next_node_seq,
            node_row_lists: HashMap::new(),
        };
        this.schedule_rebuild(cx);
        this
    }

    fn next_flow_id(&mut self, kind: &str) -> FlowId {
        let n = self.next_node_seq;
        self.next_node_seq += 1;
        SharedString::from(format!("{kind}-{n}"))
    }

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

    pub fn add_node(&mut self, descriptor: &OpDescriptor, cx: &mut Context<Self>) {
        let spec = (descriptor.default_spec)();
        self.add_node_with_spec(descriptor.label, spec, cx);
    }

    /// Spawn a node with a fully-specified `spec`. Used by the palette
    /// wizards that gather extra input (component name for `Persist`,
    /// component id for `FromDb`) before insertion.
    pub fn add_node_with_spec(&mut self, kind_label: &str, spec: NodeSpec, cx: &mut Context<Self>) {
        let (x, y) = self.next_node_position();
        let id = self.next_flow_id(kind_label);
        self.graph.update(cx, |g, _| {
            g.insert_node(id.clone(), spec, Position { x, y });
        });
        self.selection = Some(id);
        self.schedule_rebuild(cx);
        cx.notify();
    }

    /// Currently-selected node, if any.
    pub fn selected_node(&self) -> Option<&FlowId> {
        self.selection.as_ref()
    }

    /// Currently-selected edge, if any.
    pub fn selected_edge(&self) -> Option<&EdgeEntry> {
        self.selected_edge.as_ref()
    }

    /// Trigger the same delete logic the keymap fires. Removes the
    /// currently-selected edge if any, otherwise the selected node.
    pub fn delete_selection(&mut self, cx: &mut Context<Self>) {
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

    fn next_node_position(&self) -> (f32, f32) {
        // Stagger near the upper-left so it's visible on a fresh canvas; nudge
        // diagonally so successive adds don't stack.
        let n = self.next_node_seq as f32;
        (24.0 + (n % 6.0) * 24.0, 24.0 + n * 12.0)
    }

    /// Cancel any pending rebuild and queue a fresh one after the debounce
    /// window. Public so external callers (inspector arg edits, palette
    /// commands) can ask the editor to refresh without going through the
    /// internal mutation path.
    pub fn schedule_rebuild(&mut self, cx: &mut Context<Self>) {
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

    /// Build / refresh the inline `RowList` for each node currently in
    /// `alive`, dropping entries for any node that no longer exists. Called
    /// once per render before the node cards are drawn. The row list is
    /// only rebuilt (`set_rows`) when a node's spec actually changed —
    /// otherwise the existing list is preserved so any in-flight inline
    /// edit doesn't get clobbered.
    fn refresh_row_lists(&mut self, alive: &HashSet<FlowId>, cx: &mut Context<Self>) {
        self.node_row_lists.retain(|id, _| alive.contains(id));
        let editor_handle = cx.entity().clone();
        let graph_handle = self.graph.clone();
        let db = self.db.clone();
        let snapshots: Vec<(FlowId, NodeSpec)> = {
            let g = self.graph.read(cx);
            alive
                .iter()
                .filter_map(|id| g.nodes.get(id).map(|e| (id.clone(), e.spec.clone())))
                .collect()
        };
        for (flow_id, spec) in snapshots {
            let rebuild = match self.node_row_lists.get(&flow_id) {
                Some(entry) => entry.spec != spec,
                None => true,
            };
            if !rebuild {
                continue;
            }
            let rows = crate::node_editor::inspector_rows::rows_for_node(
                &editor_handle,
                &graph_handle,
                &flow_id,
                &db,
                &spec,
                cx,
            );
            if let Some(entry) = self.node_row_lists.get_mut(&flow_id) {
                entry.spec = spec;
                entry.list.update(cx, |l, cx| l.set_rows(rows, cx));
            } else {
                let list = cx.new(|cx| crate::inspector::row_list::RowList::new(rows, cx));
                self.node_row_lists
                    .insert(flow_id, NodeRowEntry { spec, list });
            }
        }
    }

    /// World-space extent of all nodes, used to bound scrolling and size
    /// the scrollbar thumbs. Includes a small margin so scrolled-to-edge
    /// nodes have breathing room.
    fn content_extent(&self, cx: &App) -> Point<f32> {
        const MARGIN: f32 = 24.0;
        let g = self.graph.read(cx);
        let mut max_x = 0.0_f32;
        let mut max_y = 0.0_f32;
        for (id, entry) in &g.nodes {
            let h = node_height(input_count(entry, &g.edges, id), args_count(&entry.spec));
            max_x = max_x.max(entry.position.x + NODE_WIDTH);
            max_y = max_y.max(entry.position.y + h);
        }
        point(max_x + MARGIN, max_y + MARGIN)
    }

    fn clamp_viewport(&mut self, cx: &App) {
        let extent = self.content_extent(cx);
        if let Some(size) = self.canvas_size {
            let vw = f32::from(size.width);
            let vh = f32::from(size.height);
            let max_x = (extent.x - vw).max(0.0);
            let max_y = (extent.y - vh).max(0.0);
            self.viewport.x = self.viewport.x.clamp(0.0, max_x);
            self.viewport.y = self.viewport.y.clamp(0.0, max_y);
        } else {
            self.viewport.x = self.viewport.x.max(0.0);
            self.viewport.y = self.viewport.y.max(0.0);
        }
    }

    fn on_delete(&mut self, _: &DeleteSelected, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_selection(cx);
    }

    /// One-shot auto-layout: run the shared layered engine over the current
    /// graph and overwrite every node's position. Manual placement stays the
    /// primary model — this just untangles the canvas on demand.
    pub fn auto_layout(&mut self, cx: &mut Context<Self>) {
        let positions = auto_layout_positions(self.graph.read(cx));
        if positions.is_empty() {
            return;
        }
        self.graph.update(cx, |g, _| {
            for (id, (x, y)) in positions {
                if let Some(entry) = g.nodes.get_mut(&id) {
                    entry.position = Position { x, y };
                }
            }
        });
        self.clamp_viewport(cx);
        cx.notify();
    }
}

/// Engine positions for every node, keyed by flow id. Deterministic despite
/// `NodeGraph::nodes` being a `HashMap`: nodes enter the engine sorted by
/// flow id, which also serves as the tie-break rank. Edge endpoints attach
/// at real socket offsets, and edges touching a cycle are left unranked so a
/// user-created loop can't inflate the layering.
pub(super) fn auto_layout_positions(g: &NodeGraph) -> Vec<(FlowId, (f32, f32))> {
    use crate::graph_layout::{
        self, LayoutEdge, LayoutInput, LayoutNode, LayoutOptions, PinAnchor,
    };

    let mut ids: Vec<FlowId> = g.nodes.keys().cloned().collect();
    ids.sort();
    let index: HashMap<&FlowId, usize> =
        ids.iter().enumerate().map(|(i, id)| (id, i)).collect();
    let sockets = |id: &FlowId| {
        let entry = &g.nodes[id];
        (
            input_count(entry, &g.edges, id),
            args_count(&entry.spec),
        )
    };
    let nodes: Vec<LayoutNode> = ids
        .iter()
        .map(|id| {
            let (inputs, args) = sockets(id);
            LayoutNode {
                size: (NODE_WIDTH, node_height(inputs, args)),
            }
        })
        .collect();
    let (_, cycle_members) = g.topo_order();
    let edges: Vec<LayoutEdge> = g
        .edges
        .iter()
        .filter_map(|e| {
            let (from, to) = (*index.get(&e.source)?, *index.get(&e.target)?);
            let (inputs, args) = sockets(&e.source);
            Some(LayoutEdge {
                from,
                to,
                from_pin: PinAnchor::Offset(output_socket_local_y(inputs, args)),
                to_pin: PinAnchor::Offset(input_socket_local_y(e.target_socket)),
                ranked: !cycle_members.contains(&e.source) && !cycle_members.contains(&e.target),
            })
        })
        .collect();
    let tie_break: Vec<usize> = (0..ids.len()).collect();
    let out = graph_layout::compute(&LayoutInput {
        nodes: &nodes,
        edges: &edges,
        tie_break: &tie_break,
        options: LayoutOptions {
            layer_gap: 80.0,
            node_gap: 24.0,
            margin: 24.0,
            ..LayoutOptions::default()
        },
    });
    ids.into_iter()
        .enumerate()
        .map(|(i, id)| (id, out.positions[i]))
        .collect()
}

impl Focusable for NodeEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Number of input sockets shown on `entry`. Variadic ops always render one
/// trailing empty socket past the highest connected one, so dropping there
/// extends the input list with a fresh slot. Order is `target_socket` —
/// the user controls input ordering by which socket they wire into.
fn input_count(entry: &NodeEntry, edges: &[EdgeEntry], flow_id: &FlowId) -> usize {
    match descriptor_for(&entry.spec).inputs {
        Arity::Exact(slots) => slots.len(),
        Arity::Variadic { min, .. } => {
            let connected = edges.iter().filter(|e| &e.target == flow_id).count();
            (connected + 1).max(min)
        }
    }
}

/// Inline arg row height — matches `row_base`'s 28px so node height
/// math agrees with what gpui actually paints.
const ARG_ROW_HEIGHT: f32 = 28.0;

/// Vertical padding (top + bottom) added by `RowList` around its rows.
/// Mirrored here so `node_height` stays in sync with what gets painted.
const ARG_LIST_PAD: f32 = 4.0;

/// Number of inline arg rows for a spec, used to size the node card.
fn args_count(spec: &NodeSpec) -> usize {
    descriptor_for(spec).arg_count
}

/// Height of a node card. Header on top, then `inputs * SOCKET_ROW_HEIGHT`
/// of input-socket lane, then `arg_count` rows for inline editing. Empty
/// nodes (no inputs, no args) reserve one socket row so the card doesn't
/// collapse to just a header.
fn node_height(input_count: usize, arg_count: usize) -> f32 {
    let body = if input_count == 0 && arg_count == 0 {
        SOCKET_ROW_HEIGHT
    } else {
        let args_h = if arg_count == 0 {
            0.0
        } else {
            (arg_count as f32) * ARG_ROW_HEIGHT + ARG_LIST_PAD
        };
        (input_count as f32) * SOCKET_ROW_HEIGHT + args_h
    };
    HEADER_HEIGHT + body
}

fn input_socket_local_y(socket_index: usize) -> f32 {
    HEADER_HEIGHT + (socket_index as f32 + 0.5) * SOCKET_ROW_HEIGHT
}

fn output_socket_local_y(input_count: usize, arg_count: usize) -> f32 {
    node_height(input_count, arg_count) / 2.0
}

/// Color a socket dot by its declared kind.
fn socket_dot_color(kind: SocketKind, theme: &crate::theme::Theme) -> Hsla {
    match kind {
        SocketKind::Clock => theme.line_colors[1], // cool blue family
        SocketKind::F64Scalar => theme.line_colors[0], // accent orange
        SocketKind::Value => theme.line_colors[2], // green — distinct from scalar
        SocketKind::Any => theme.text_secondary,
    }
}

fn edge_color_to_hsla(c: EdgeColor, theme: &crate::theme::Theme) -> Hsla {
    match c {
        EdgeColor::SharedClock(idx) => theme.line_colors[idx % theme.line_colors.len()],
        EdgeColor::Neutral => theme.border_primary,
        EdgeColor::Error => theme.error_accent,
        EdgeColor::Pending => theme.edge_pending(),
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

impl Render for NodeEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme(cx);
        let graph_ref = self.graph.read(cx);
        let viewport = self.viewport.clone();
        let selected_edge = self.selected_edge.clone();

        // Per-node screen position = graph position - viewport offset.
        let mut node_snapshots: Vec<NodeRenderSnapshot> = Vec::with_capacity(graph_ref.nodes.len());
        for (id, entry) in &graph_ref.nodes {
            node_snapshots.push(NodeRenderSnapshot {
                flow_id: id.clone(),
                graph_pos: entry.position.clone(),
                screen_pos: Position {
                    x: entry.position.x - viewport.x,
                    y: entry.position.y - viewport.y,
                },
                descriptor: descriptor_for(&entry.spec),
                inputs: input_count(entry, &graph_ref.edges, id),
                args: args_count(&entry.spec),
                pill: status_pill(entry),
            });
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
                let s_args = args_count(&source.spec);
                let src_x = source.position.x - viewport.x + NODE_WIDTH;
                let src_y =
                    source.position.y - viewport.y + output_socket_local_y(s_inputs, s_args);
                let tgt_x = target.position.x - viewport.x;
                let tgt_y =
                    target.position.y - viewport.y + input_socket_local_y(edge.target_socket);
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
            let s_args = args_count(&source.spec);
            let src_x = source.position.x - viewport.x + NODE_WIDTH;
            let src_y = source.position.y - viewport.y + output_socket_local_y(s_inputs, s_args);
            Some((
                point(px(src_x), px(src_y)),
                draft.pointer,
                theme.text_secondary,
            ))
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
                        this.canvas_size = Some(bounds.size);
                    });
                    bounds
                }
            },
            move |_, bounds, window, _cx| {
                paint_grid(bounds, theme_for_paint.grid_color, window);
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

        let edge_geometry: Arc<Vec<(EdgeEntry, RoutePoints)>> = Arc::new(
            edge_snapshots
                .iter()
                .map(|(edge, s, t, _)| (edge.clone(), RoutePoints::from_slice(&[*s, *t])))
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
                    if let Some(edge) = hit_test_edges(&edges, local, gpui::Axis::Horizontal) {
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
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &gpui::MouseDownEvent, window, cx| {
                    let editor = cx.entity().clone();
                    window.dispatch_action(
                        Box::new(crate::inspector::InspectEntity {
                            entity: editor.into_any(),
                            position: ev.position,
                        }),
                        cx,
                    );
                    cx.stop_propagation();
                    let _ = this;
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &gpui::MouseMoveEvent, _w, cx| {
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
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _ev: &gpui::MouseUpEvent, _w, cx| {
                    this.node_drag.take();
                    if this.edge_draft.take().is_some() {
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
            .on_scroll_wheel(
                cx.listener(|this, event: &gpui::ScrollWheelEvent, _window, cx| {
                    let delta = event.delta.pixel_delta(px(20.0));
                    // viewport.{x,y} are positive when scrolled into content;
                    // wheel delta is negative when scrolling down, so subtract.
                    this.viewport.x -= f32::from(delta.x);
                    this.viewport.y -= f32::from(delta.y);
                    this.clamp_viewport(cx);
                    cx.notify();
                }),
            )
            .child(canvas_layer);

        // Refresh per-node `RowList`s so each node card has up-to-date
        // inline arg rows. Done after node_snapshots are computed so we
        // know which flow ids are alive, but before render_node so the
        // cards can pull the row-list entity by id.
        let alive_ids: HashSet<FlowId> = node_snapshots.iter().map(|s| s.flow_id.clone()).collect();
        self.refresh_row_lists(&alive_ids, cx);

        for snap in node_snapshots {
            let row_list = self
                .node_row_lists
                .get(&snap.flow_id)
                .map(|e| e.list.clone());
            root = root.child(self.render_node(snap, row_list, &theme, cx));
        }

        // Scrollbars (indicator-only — wheel handler drives the viewport).
        if let Some(size) = self.canvas_size {
            let extent = self.content_extent(cx);
            let vw = f32::from(size.width);
            let vh = f32::from(size.height);
            root = root
                .child(crate::views::scrollbar::Scrollbar::new(
                    gpui::Axis::Vertical,
                    vh,
                    extent.y,
                    self.viewport.y,
                ))
                .child(crate::views::scrollbar::Scrollbar::new(
                    gpui::Axis::Horizontal,
                    vw,
                    extent.x,
                    self.viewport.x,
                ));
        }

        // Edge-delete "×" affordance painted near the midpoint of the
        // currently selected edge.
        if let Some(selected) = self.selected_edge.as_ref() {
            for (edge, src, tgt, _color) in &edge_snapshots {
                if edge != selected {
                    continue;
                }
                let mid_x = (f32::from(src.x) + f32::from(tgt.x)) / 2.0;
                let mid_y = (f32::from(src.y) + f32::from(tgt.y)) / 2.0;
                let size = 14.0_f32;
                let edge_for_click = edge.clone();
                root = root.child(
                    div()
                        .absolute()
                        .left(px(mid_x - size / 2.0))
                        .top(px(mid_y - size / 2.0))
                        .w(px(size))
                        .h(px(size))
                        .rounded_full()
                        .bg(theme.bg_elevated)
                        .border_1()
                        .border_color(theme.line_colors[0])
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(10.0))
                        .text_color(theme.line_colors[0])
                        .child("×")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _ev: &gpui::MouseDownEvent, _w, cx| {
                                let edge = edge_for_click.clone();
                                this.graph.update(cx, |g, _| g.remove_edge(&edge));
                                this.selected_edge = None;
                                this.schedule_rebuild(cx);
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        ),
                );
                break;
            }
        }

        root
    }
}

impl NodeEditor {
    fn render_node(
        &self,
        snap: NodeRenderSnapshot,
        row_list: Option<Entity<crate::inspector::row_list::RowList>>,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let NodeRenderSnapshot {
            flow_id,
            graph_pos,
            screen_pos,
            descriptor,
            inputs,
            args,
            pill: (pill_text, pill_error),
        } = snap;
        let height = node_height(inputs, args);
        let selected = self.selection.as_ref() == Some(&flow_id);

        let border_color = if selected {
            theme.line_colors[0]
        } else {
            theme.border_primary
        };
        let pill_color = if pill_error {
            theme.error_accent
        } else {
            theme.text_secondary
        };

        // Outer wrapper is positioned absolutely on the canvas and hosts the
        // bordered card *and* the socket dots as siblings — so the dots paint
        // on top of (rather than being clipped by) the card's border.
        let body = div()
            .absolute()
            .left(px(screen_pos.x))
            .top(px(screen_pos.y))
            .w(px(NODE_WIDTH))
            .h(px(height));

        let card = div()
            .size_full()
            .bg(theme.bg_elevated)
            .border_1()
            .border_color(border_color)
            .rounded_md()
            .overflow_hidden()
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
                        // Right-click on a node opens the standard Inspector
                        // anchored at the click — gives access to the full
                        // row set (including Cascade pickers) in the
                        // familiar palette UX. The inline row list inside
                        // the card handles quick Scalar/Text edits without
                        // needing this overlay.
                        this.selection = Some(id.clone());
                        let editor_entity = cx.entity().clone();
                        let graph_entity = this.graph.clone();
                        let proxy =
                            cx.new(|_| crate::node_editor::inspector_rows::SelectedNodeProxy {
                                editor: editor_entity,
                                graph: graph_entity,
                                flow_id: id.clone(),
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
            )
            // Header bar: label + status pill.
            .child(
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

        let mut body = body.child(card);

        // Input sockets (left edge) — siblings of `card` so they paint on top
        // of the card border.
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

        // Inline row list (arg fields), positioned directly below the
        // input-socket lane. Spans the full card width so the row's own
        // padding (12px in `row_base`) is what aligns the labels — no
        // extra margin from the wrapper.
        if let Some(list) = row_list
            && args > 0
        {
            let list_top = HEADER_HEIGHT + (inputs as f32) * SOCKET_ROW_HEIGHT;
            body = body.child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(list_top))
                    .child(list),
            );
        }

        // Output socket (right edge).
        let out_y = output_socket_local_y(inputs, args) - SOCKET_DOT_SIZE / 2.0;
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
