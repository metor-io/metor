//! Deterministic layered auto-layout for the system graph.
//!
//! The layout is a pure function of the [`Wiring`] IR and the set of collapsed
//! scopes, so it is unit-tested without any gpui state. Nodes are one card per
//! system, per slot, per collapsed-scope group, plus the reserved coordinator
//! when an edge references it. Layers run left→right over the *non-delayed
//! frame* edges only — delayed feedback edges and message edges are excluded
//! from layering (and drawn as back/side edges) so the acyclic skeleton drives
//! the columns. Within a layer, a barycenter pass orders nodes to reduce
//! obvious crossings; ties break by id so the result is stable.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use gpui::SharedString;
use metor_fsw_2::ir::{EdgeKind, Wiring};

/// Reserved instance name the coordinator registers under. It never appears in
/// [`Wiring::systems`] (it is registered at runtime), so an edge naming it is
/// the only evidence it participates in the graph. Contract string mirrored
/// from `metor-fsw-2`'s coordinator, which does not export it.
pub const COORDINATOR_INSTANCE: &str = "coordinator";

/// Card width; also the layout's horizontal unit.
pub const NODE_WIDTH: f32 = 188.0;
/// Horizontal gap between adjacent layers.
const LAYER_GAP: f32 = 104.0;
/// Vertical pitch between nodes within a layer.
const NODE_PITCH: f32 = 104.0;
/// Margin from the canvas origin to the first node.
const MARGIN: f32 = 32.0;

/// What a laid-out node stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphNodeKind {
    System,
    Slot,
    Coordinator,
    /// A collapsed scope aggregating its members into one card.
    ScopeGroup,
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

/// The laid-out graph: positioned nodes and rerouted edges.
#[derive(Debug)]
pub struct GraphLayout {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
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
pub fn layout(wiring: &Wiring, collapsed: &BTreeSet<String>) -> GraphLayout {
    // Instance name -> the visible node that represents it.
    let mut rep: HashMap<String, SharedString> = HashMap::new();
    // Ordered, de-duplicated node builders keyed by id.
    let mut node_kind: BTreeMap<SharedString, (GraphNodeKind, Option<usize>)> = BTreeMap::new();
    // First-seen order so a flat mission keeps declaration order as a tiebreak.
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
            see(&r, GraphNodeKind::System, Some(i), &mut node_kind, &mut first_seen);
        } else {
            see(&r, GraphNodeKind::ScopeGroup, None, &mut node_kind, &mut first_seen);
        }
    }
    for (i, slot) in wiring.slots.iter().enumerate() {
        let r = representative(wiring, &slot.name, slot.scope, collapsed);
        rep.insert(slot.name.clone(), r.clone());
        if r == slot.name.as_str() {
            see(&r, GraphNodeKind::Slot, Some(i), &mut node_kind, &mut first_seen);
        } else {
            see(&r, GraphNodeKind::ScopeGroup, None, &mut node_kind, &mut first_seen);
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
    let mut seen_edges: BTreeSet<(SharedString, SharedString, SharedString, SharedString, bool, bool)> =
        BTreeSet::new();
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
    let index: HashMap<SharedString, usize> =
        ids.iter().cloned().enumerate().map(|(i, id)| (id, i)).collect();

    // Layered skeleton: non-delayed frame edges only.
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); ids.len()];
    let mut pred: Vec<Vec<usize>> = vec![Vec::new(); ids.len()];
    let mut skeleton: Vec<(usize, usize)> = Vec::new();
    for e in &edges {
        if e.kind != EdgeKind::Frame || e.delayed {
            continue;
        }
        let (u, v) = (index[&e.from_node], index[&e.to_node]);
        succ[u].push(v);
        pred[v].push(u);
        skeleton.push((u, v));
    }
    skeleton.sort_unstable();

    let layers = assign_layers(ids.len(), &skeleton);
    let orders = order_within_layers(&ids, &layers, &pred, &succ);

    let nodes: Vec<GraphNode> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let (kind, src) = node_kind[id];
            let layer = layers[i];
            let order = orders[i];
            GraphNode {
                id: id.clone(),
                kind,
                source_index: src,
                layer,
                pos: (
                    MARGIN + layer as f32 * (NODE_WIDTH + LAYER_GAP),
                    MARGIN + order as f32 * NODE_PITCH,
                ),
            }
        })
        .collect();

    GraphLayout { nodes, edges }
}

/// Longest-path layering over the acyclic skeleton. Relaxation is capped at
/// `n` passes so an unexpected residual cycle terminates deterministically
/// instead of looping (the delayed edges that would form real cycles are
/// already excluded).
fn assign_layers(n: usize, skeleton: &[(usize, usize)]) -> Vec<usize> {
    let mut layer = vec![0usize; n];
    for _ in 0..n.max(1) {
        let mut changed = false;
        for &(u, v) in skeleton {
            if layer[v] < layer[u] + 1 {
                layer[v] = layer[u] + 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    layer
}

/// Assign a within-layer order to every node, minimizing crossings with a
/// barycenter heuristic. Initial order is by id (via first-seen index already
/// baked into `pred`/`succ` adjacency); a few forward/back sweeps refine it.
/// Ties break by id so the output is deterministic.
fn order_within_layers(
    ids: &[SharedString],
    layers: &[usize],
    pred: &[Vec<usize>],
    succ: &[Vec<usize>],
) -> Vec<usize> {
    let max_layer = layers.iter().copied().max().unwrap_or(0);
    // Nodes grouped by layer, initialized in id order for determinism.
    let mut by_layer: Vec<Vec<usize>> = vec![Vec::new(); max_layer + 1];
    let mut node_ix: Vec<usize> = (0..ids.len()).collect();
    node_ix.sort_by(|&a, &b| ids[a].cmp(&ids[b]));
    for &n in &node_ix {
        by_layer[layers[n]].push(n);
    }

    // Current order index of each node within its layer.
    let mut order: Vec<usize> = vec![0; ids.len()];
    let refresh = |by_layer: &[Vec<usize>], order: &mut [usize]| {
        for layer_nodes in by_layer {
            for (pos, &n) in layer_nodes.iter().enumerate() {
                order[n] = pos;
            }
        }
    };
    refresh(&by_layer, &mut order);

    let barycenter = |n: usize, neighbors: &[usize], order: &[usize]| -> f32 {
        if neighbors.is_empty() {
            return order[n] as f32;
        }
        let sum: usize = neighbors.iter().map(|&m| order[m]).sum();
        sum as f32 / neighbors.len() as f32
    };

    for _ in 0..4 {
        // Forward: order each layer by the barycenter of its predecessors.
        for l in 1..=max_layer {
            let mut layer_nodes = std::mem::take(&mut by_layer[l]);
            layer_nodes.sort_by(|&a, &b| {
                let ba = barycenter(a, &pred[a], &order);
                let bb = barycenter(b, &pred[b], &order);
                ba.partial_cmp(&bb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| ids[a].cmp(&ids[b]))
            });
            by_layer[l] = layer_nodes;
            refresh(&by_layer, &mut order);
        }
        // Backward: order by the barycenter of successors.
        for l in (0..max_layer).rev() {
            let mut layer_nodes = std::mem::take(&mut by_layer[l]);
            layer_nodes.sort_by(|&a, &b| {
                let ba = barycenter(a, &succ[a], &order);
                let bb = barycenter(b, &succ[b], &order);
                ba.partial_cmp(&bb)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| ids[a].cmp(&ids[b]))
            });
            by_layer[l] = layer_nodes;
            refresh(&by_layer, &mut order);
        }
    }

    order
}
