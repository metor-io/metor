//! Wiring-IR → laid-out graph for the system graph tile.
//!
//! This module owns the *domain* half of layout: which nodes exist (one card
//! per system, per slot, per collapsed-scope group, plus the reserved
//! coordinator when an edge references it), and which drawable edges connect
//! them after collapsed scopes swallow their members. The *geometry* half —
//! layers, ordering, positions, wire routes — is delegated to the shared
//! [`graph_layout`](crate::graph_layout) engine, driven by the non-delayed
//! frame edges (the target's acyclic execution-dependency DAG) with
//! declaration order as the tie-break. The result is a pure function of the
//! [`Wiring`] IR, the collapsed set, and the flow direction, so it is
//! unit-tested without any gpui state.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use gpui::SharedString;
use metor_fsw_2::ir::{EdgeKind, Wiring};

use crate::graph_layout::{
    self, Direction, EdgeRoute, LayoutEdge, LayoutInput, LayoutNode, LayoutOptions, PinAnchor,
};

/// Reserved instance name the coordinator registers under. It never appears in
/// [`Wiring::systems`] (it is registered at runtime), so an edge naming it is
/// the only evidence it participates in the graph. Contract string mirrored
/// from `metor-fsw-2`'s coordinator, which does not export it.
pub const COORDINATOR_INSTANCE: &str = "coordinator";

/// Card width, shared by every node kind.
pub const NODE_WIDTH: f32 = 188.0;

/// What a laid-out node stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphNodeKind {
    System,
    Slot,
    Coordinator,
    /// A collapsed scope aggregating its members into one card.
    ScopeGroup,
}

/// Card footprint per node kind. Fixed per kind so the card renders at
/// exactly the size the layout and wire anchors assume.
pub fn card_size(kind: GraphNodeKind) -> (f32, f32) {
    let height = match kind {
        GraphNodeKind::System => 62.0,
        GraphNodeKind::Slot => 84.0,
        GraphNodeKind::Coordinator => 44.0,
        GraphNodeKind::ScopeGroup => 48.0,
    };
    (NODE_WIDTH, height)
}

/// One positioned node in the laid-out graph.
#[derive(Clone, Debug)]
pub struct GraphNode {
    /// Instance name for a system/slot/coordinator, or the scope path for a
    /// collapsed group. Also the edge-rerouting key and the selection id.
    pub id: SharedString,
    pub kind: GraphNodeKind,
    /// Index into [`Wiring::systems`] or [`Wiring::slots`] for detail lookup;
    /// `None` for the coordinator and for scope groups.
    pub source_index: Option<usize>,
    pub layer: usize,
    /// Graph-space top-left before any manual override is applied.
    pub pos: (f32, f32),
}

/// One edge, with endpoints already rerouted to the visible node that
/// represents each side (a collapsed scope's members point at the group).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphEdge {
    pub from_node: SharedString,
    pub to_node: SharedString,
    pub from_port: SharedString,
    pub to_port: SharedString,
    pub kind: EdgeKind,
    pub delayed: bool,
}

/// The laid-out graph: positioned nodes, rerouted edges, and a wire route per
/// edge (indexed like `edges`).
#[derive(Debug)]
pub struct GraphLayout {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    pub routes: Vec<EdgeRoute>,
}

impl GraphLayout {
    pub fn node(&self, id: &str) -> Option<&GraphNode> {
        self.nodes.iter().find(|n| n.id == id)
    }
}

/// The chain of scope indices from `scope` up to the root, self first.
fn scope_chain(wiring: &Wiring, scope: Option<usize>) -> Vec<usize> {
    let mut chain = Vec::new();
    let mut cur = scope;
    while let Some(idx) = cur {
        let Some(spec) = wiring.scopes.get(idx) else {
            break;
        };
        chain.push(idx);
        cur = spec.parent;
    }
    chain
}

/// The visible representative of an instance in `scope`: the topmost collapsed
/// scope on its ancestor chain (the group that swallows it), or the instance's
/// own name when nothing on the chain is collapsed.
fn representative(
    wiring: &Wiring,
    instance: &str,
    scope: Option<usize>,
    collapsed: &BTreeSet<String>,
) -> SharedString {
    let chain = scope_chain(wiring, scope);
    // Root-ward: the first collapsed scope encountered is the topmost one.
    for &idx in chain.iter().rev() {
        let path = &wiring.scopes[idx].path;
        if collapsed.contains(path) {
            return SharedString::from(path.clone());
        }
    }
    SharedString::from(instance.to_string())
}

/// Compute the full layout for `wiring` with the given collapsed scopes.
pub fn layout(wiring: &Wiring, collapsed: &BTreeSet<String>, direction: Direction) -> GraphLayout {
    // Instance name -> the visible node that represents it.
    let mut rep: HashMap<String, SharedString> = HashMap::new();
    // Ordered, de-duplicated node builders keyed by id.
    let mut node_kind: BTreeMap<SharedString, (GraphNodeKind, Option<usize>)> = BTreeMap::new();
    // First-seen order: declaration order for a flat target and layout input.
    let mut first_seen: Vec<SharedString> = Vec::new();

    let see = |id: &SharedString,
               kind: GraphNodeKind,
               src: Option<usize>,
               node_kind: &mut BTreeMap<SharedString, (GraphNodeKind, Option<usize>)>,
               first_seen: &mut Vec<SharedString>| {
        if node_kind.insert(id.clone(), (kind, src)).is_none() {
            first_seen.push(id.clone());
        }
    };

    for (i, sys) in wiring.systems.iter().enumerate() {
        let r = representative(wiring, &sys.name, sys.scope, collapsed);
        rep.insert(sys.name.clone(), r.clone());
        if r == sys.name.as_str() {
            see(
                &r,
                GraphNodeKind::System,
                Some(i),
                &mut node_kind,
                &mut first_seen,
            );
        } else {
            see(
                &r,
                GraphNodeKind::ScopeGroup,
                None,
                &mut node_kind,
                &mut first_seen,
            );
        }
    }
    for (i, slot) in wiring.slots.iter().enumerate() {
        let r = representative(wiring, &slot.name, slot.scope, collapsed);
        rep.insert(slot.name.clone(), r.clone());
        if r == slot.name.as_str() {
            see(
                &r,
                GraphNodeKind::Slot,
                Some(i),
                &mut node_kind,
                &mut first_seen,
            );
        } else {
            see(
                &r,
                GraphNodeKind::ScopeGroup,
                None,
                &mut node_kind,
                &mut first_seen,
            );
        }
    }

    // Coordinator and any other endpoint not declared as a system/slot: create
    // a node so no edge is dropped. The coordinator is untyped and unscoped.
    let resolve = |name: &str,
                   rep: &HashMap<String, SharedString>,
                   node_kind: &mut BTreeMap<SharedString, (GraphNodeKind, Option<usize>)>,
                   first_seen: &mut Vec<SharedString>|
     -> SharedString {
        if let Some(r) = rep.get(name) {
            return r.clone();
        }
        let id = SharedString::from(name.to_string());
        let kind = if name == COORDINATOR_INSTANCE {
            GraphNodeKind::Coordinator
        } else {
            GraphNodeKind::System
        };
        if node_kind.insert(id.clone(), (kind, None)).is_none() {
            first_seen.push(id.clone());
        }
        id
    };

    let mut edges: Vec<GraphEdge> = Vec::new();
    let mut seen_edges: BTreeSet<(
        SharedString,
        SharedString,
        SharedString,
        SharedString,
        bool,
        bool,
    )> = BTreeSet::new();
    for e in &wiring.edges {
        let from_node = resolve(&e.from, &rep, &mut node_kind, &mut first_seen);
        let to_node = resolve(&e.to, &rep, &mut node_kind, &mut first_seen);
        // An edge internal to one collapsed group carries no cross-node
        // information, so drop it rather than draw a self-loop.
        if from_node == to_node {
            continue;
        }
        let is_msg = e.kind == EdgeKind::Msg;
        let key = (
            from_node.clone(),
            to_node.clone(),
            SharedString::from(e.out.clone()),
            SharedString::from(e.in_.clone()),
            is_msg,
            e.delayed,
        );
        if !seen_edges.insert(key) {
            continue;
        }
        edges.push(GraphEdge {
            from_node,
            to_node,
            from_port: e.out.clone().into(),
            to_port: e.in_.clone().into(),
            kind: e.kind,
            delayed: e.delayed,
        });
    }

    // Stable node index space, in first-seen order.
    let ids: Vec<SharedString> = first_seen;
    let index: HashMap<SharedString, usize> = ids
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, id)| (id, i))
        .collect();

    let layout_nodes: Vec<LayoutNode> = ids
        .iter()
        .map(|id| LayoutNode {
            size: card_size(node_kind[id].0),
        })
        .collect();
    // The non-delayed frame edges are the target's execution-dependency DAG;
    // they alone drive layering. Delayed feedback and message edges route by
    // wherever those layers put their endpoints.
    let layout_edges: Vec<LayoutEdge> = edges
        .iter()
        .map(|e| LayoutEdge {
            from: index[&e.from_node],
            to: index[&e.to_node],
            from_pin: PinAnchor::Auto,
            to_pin: PinAnchor::Auto,
            ranked: e.kind == EdgeKind::Frame && !e.delayed,
        })
        .collect();
    let out = graph_layout::compute(&LayoutInput {
        nodes: &layout_nodes,
        edges: &layout_edges,
        options: LayoutOptions {
            direction,
            ..LayoutOptions::default()
        },
    });

    let nodes: Vec<GraphNode> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let (kind, src) = node_kind[id];
            GraphNode {
                id: id.clone(),
                kind,
                source_index: src,
                layer: out.layers[i],
                pos: out.positions[i],
            }
        })
        .collect();

    GraphLayout {
        nodes,
        edges,
        routes: out.routes,
    }
}
