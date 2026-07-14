//! Crossing reduction: the within-layer order of nodes, computed over an
//! arena that includes one virtual node per intermediate layer of every long
//! forward edge. Virtual nodes are what let a layer-spanning edge take part
//! in ordering (so it flows through a corridor instead of slicing across
//! cards) and later reserve a wire channel between the real cards.

use super::{LayoutEdge, NodeIx};

/// How an edge relates to the layering, decided by its endpoints' layers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EdgeClass {
    /// Runs one or more layers forward; participates in ordering, with a
    /// virtual node per intermediate layer.
    Forward,
    /// Both endpoints share a layer.
    Flat,
    /// Runs backward, or was declared a feedback edge.
    Back,
}

/// The expanded layered graph. Vnode indices `0..node_count` are the real
/// nodes (same index space as the input); higher indices are virtual.
pub(super) struct Arena {
    /// Original node index for real vnodes, `None` for virtual ones.
    pub orig: Vec<Option<NodeIx>>,
    /// Layer per vnode.
    pub layer: Vec<usize>,
    /// Vnodes per layer, in final crossing-reduced order.
    pub by_layer: Vec<Vec<usize>>,
    /// Per input edge: its class, and its virtual chain in flow order (empty
    /// unless [`EdgeClass::Forward`] spanning more than one layer).
    pub class: Vec<EdgeClass>,
    pub dummies: Vec<Vec<usize>>,
    /// Unit-span adjacency over vnodes (forward edges only).
    pub pred: Vec<Vec<usize>>,
    pub succ: Vec<Vec<usize>>,
}

impl Arena {
    pub fn build(
        node_count: usize,
        edges: &[LayoutEdge],
        layers: &[usize],
        tie_break: &[usize],
    ) -> Self {
        let mut orig: Vec<Option<NodeIx>> = (0..node_count).map(Some).collect();
        let mut layer: Vec<usize> = layers.to_vec();
        // Deterministic sort key per vnode: real nodes by the caller's
        // declaration order, virtual nodes by (owning edge, segment).
        let mut tie: Vec<(usize, usize, usize)> =
            (0..node_count).map(|i| (0, tie_break[i], 0)).collect();
        let mut class = Vec::with_capacity(edges.len());
        let mut dummies: Vec<Vec<usize>> = vec![Vec::new(); edges.len()];
        let mut pred: Vec<Vec<usize>> = vec![Vec::new(); node_count];
        let mut succ: Vec<Vec<usize>> = vec![Vec::new(); node_count];

        for (ei, e) in edges.iter().enumerate() {
            let (lu, lv) = (layers[e.from], layers[e.to]);
            let c = if e.back || lv < lu {
                EdgeClass::Back
            } else if lv == lu {
                EdgeClass::Flat
            } else {
                EdgeClass::Forward
            };
            class.push(c);
            if c != EdgeClass::Forward {
                continue;
            }
            let mut prev = e.from;
            for (seg, l) in (lu + 1..lv).enumerate() {
                let d = orig.len();
                orig.push(None);
                layer.push(l);
                tie.push((1, ei, seg));
                pred.push(vec![prev]);
                succ.push(Vec::new());
                succ[prev].push(d);
                dummies[ei].push(d);
                prev = d;
            }
            succ[prev].push(e.to);
            pred[e.to].push(prev);
        }

        let max_layer = layer.iter().copied().max().unwrap_or(0);
        let mut by_layer: Vec<Vec<usize>> = vec![Vec::new(); max_layer + 1];
        let mut vs: Vec<usize> = (0..orig.len()).collect();
        vs.sort_by(|&a, &b| tie[a].cmp(&tie[b]));
        for v in vs {
            by_layer[layer[v]].push(v);
        }

        let mut arena = Self {
            orig,
            layer,
            by_layer,
            class,
            dummies,
            pred,
            succ,
        };
        arena.reduce_crossings(&tie);
        arena
    }

    /// Barycenter heuristic: a few alternating sweeps, each re-sorting a
    /// layer by the mean order of its neighbors in the already-ordered
    /// adjacent layer. Ties keep the deterministic build key.
    fn reduce_crossings(&mut self, tie: &[(usize, usize, usize)]) {
        let max_layer = self.by_layer.len() - 1;
        let mut order = vec![0usize; self.orig.len()];
        let refresh = |by_layer: &[Vec<usize>], order: &mut [usize]| {
            for layer_nodes in by_layer {
                for (pos, &v) in layer_nodes.iter().enumerate() {
                    order[v] = pos;
                }
            }
        };
        refresh(&self.by_layer, &mut order);

        // A node with no neighbors keeps its current position.
        let barycenter = |v: usize, neighbors: &[usize], order: &[usize]| -> f32 {
            if neighbors.is_empty() {
                return order[v] as f32;
            }
            let sum: usize = neighbors.iter().map(|&m| order[m]).sum();
            sum as f32 / neighbors.len() as f32
        };

        for _ in 0..4 {
            for l in 1..=max_layer {
                let mut layer_nodes = std::mem::take(&mut self.by_layer[l]);
                layer_nodes.sort_by(|&a, &b| {
                    let ba = barycenter(a, &self.pred[a], &order);
                    let bb = barycenter(b, &self.pred[b], &order);
                    ba.total_cmp(&bb).then_with(|| tie[a].cmp(&tie[b]))
                });
                self.by_layer[l] = layer_nodes;
                refresh(&self.by_layer, &mut order);
            }
            for l in (0..max_layer).rev() {
                let mut layer_nodes = std::mem::take(&mut self.by_layer[l]);
                layer_nodes.sort_by(|&a, &b| {
                    let ba = barycenter(a, &self.succ[a], &order);
                    let bb = barycenter(b, &self.succ[b], &order);
                    ba.total_cmp(&bb).then_with(|| tie[a].cmp(&tie[b]))
                });
                self.by_layer[l] = layer_nodes;
                refresh(&self.by_layer, &mut order);
            }
        }
    }
}
