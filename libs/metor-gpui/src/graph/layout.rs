//! Layered (Sugiyama-style) layout of a directed graph, with layers running left to right.
//!
//! The pipeline is: resolve and deduplicate edges, reverse DFS back edges, longest-path
//! layering, dummy nodes for long edges, barycenter crossing reduction, then vertical placement
//! by repeated least-squares alignment under separation constraints. Everything is index based
//! and iterates in input order, so equal input gives bit-identical output.

/// One node to place: its caller id and its drawn size.
#[derive(Clone, Copy, Debug)]
pub struct NodeSpec {
    pub id: u64,
    pub width: f32,
    pub height: f32,
}

/// Gaps between layers (horizontal) and between nodes in a layer (vertical).
#[derive(Clone, Copy, Debug)]
pub struct Spacing {
    pub layer_gap: f32,
    pub node_gap: f32,
}

impl Default for Spacing {
    fn default() -> Self {
        Self {
            layer_gap: 48.0,
            node_gap: 16.0,
        }
    }
}

/// Where one node landed: its top-left corner, its size, its layer and its rank in that layer.
///
/// `order` counts only real nodes, top to bottom, so it is dense within a layer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub layer: usize,
    pub order: usize,
}

/// The polyline of one edge.
#[derive(Clone, Debug, PartialEq)]
pub struct Route {
    pub from: u64,
    pub to: u64,
    /// Source port, then an entry and an exit point per layer the edge passes through, then the
    /// target port.
    pub points: Vec<(f32, f32)>,
    /// True when the edge was reversed to break a cycle.
    pub reversed: bool,
}

/// A computed drawing.
#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// One placement per input node, in input order.
    pub nodes: Vec<Placement>,
    /// One route per kept input edge, in input order.
    pub routes: Vec<Route>,
    pub width: f32,
    pub height: f32,
}

/// Barycenter sweeps in crossing reduction; alternating down and up.
const ORDER_SWEEPS: usize = 8;

/// Down-and-up passes of vertical placement before the final two-sided pass.
const PLACE_PASSES: usize = 6;

/// Pull of a dummy towards its neighbour, relative to a real node, so long edges run straight.
const DUMMY_WEIGHT: f32 = 8.0;

/// Pull of a node with no neighbour on the side being aligned, so it mostly stays put.
const LONE_WEIGHT: f32 = 0.05;

impl Layout {
    /// Lays out `nodes` and `edges` (pairs of node ids) with the given spacing.
    ///
    /// Edges naming an unknown id, self-loops and repeats of an earlier edge are skipped. When
    /// two nodes share an id, edges attach to the first.
    pub fn compute(nodes: &[NodeSpec], edges: &[(u64, u64)], spacing: Spacing) -> Layout {
        if nodes.is_empty() {
            return Layout {
                nodes: Vec::new(),
                routes: Vec::new(),
                width: 0.0,
                height: 0.0,
            };
        }
        let kept = resolve_edges(nodes, edges);
        let reversed = break_cycles(nodes.len(), &kept);
        let dag: Vec<(usize, usize)> = kept
            .iter()
            .zip(&reversed)
            .map(|(&(_, u, v), &r)| if r { (v, u) } else { (u, v) })
            .collect();
        let layer = assign_layers(nodes.len(), &dag);
        let (graph, paths) = Graph::build(nodes, &layer, &dag);
        let chains: Vec<Chain> = kept
            .iter()
            .zip(reversed)
            .zip(paths)
            .map(|((&(edge, _, _), reversed), nodes)| Chain {
                edge,
                reversed,
                nodes,
            })
            .collect();
        let layers = order_layers(&graph);
        let center = place_vertically(&graph, &layers, spacing.node_gap);
        assemble(nodes, edges, spacing, &graph, &layers, &center, &chains)
    }

    /// Index into `nodes` of the node at `point`, if any.
    pub fn node_at(&self, point: (f32, f32)) -> Option<usize> {
        let (px, py) = point;
        self.nodes
            .iter()
            .position(|n| px >= n.x && px <= n.x + n.width && py >= n.y && py <= n.y + n.height)
    }
}

/// Maps edges to node indices, keeping the first of each distinct pair, as
/// `(input edge index, source, target)` in input order.
fn resolve_edges(nodes: &[NodeSpec], edges: &[(u64, u64)]) -> Vec<(usize, usize, usize)> {
    // Sorted lookup instead of a HashMap; the stable sort puts the first of equal ids first.
    let mut ids: Vec<(u64, usize)> = nodes.iter().enumerate().map(|(i, n)| (n.id, i)).collect();
    ids.sort_by_key(|&(id, _)| id);
    let find = |id: u64| {
        let at = ids.partition_point(|&(x, _)| x < id);
        ids.get(at).filter(|&&(x, _)| x == id).map(|&(_, i)| i)
    };
    let resolved: Vec<(usize, usize, usize)> = edges
        .iter()
        .enumerate()
        .filter_map(|(k, &(a, b))| Some((k, find(a)?, find(b)?)))
        .filter(|&(_, u, v)| u != v)
        .collect();
    let mut first: Vec<usize> = (0..resolved.len()).collect();
    first.sort_by_key(|&i| (resolved[i].1, resolved[i].2, i));
    first.dedup_by_key(|i| (resolved[*i].1, resolved[*i].2));
    first.sort_unstable();
    first.into_iter().map(|i| resolved[i]).collect()
}

/// Marks the edges a DFS in input order finds pointing back into its stack.
fn break_cycles(n: usize, edges: &[(usize, usize, usize)]) -> Vec<bool> {
    let mut out = vec![Vec::new(); n];
    for (e, &(_, u, _)) in edges.iter().enumerate() {
        out[u].push(e);
    }
    // 0 unvisited, 1 on the stack, 2 finished.
    let mut state = vec![0u8; n];
    let mut reversed = vec![false; edges.len()];
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for root in 0..n {
        if state[root] != 0 {
            continue;
        }
        state[root] = 1;
        stack.push((root, 0));
        while let Some(&mut (u, ref mut next)) = stack.last_mut() {
            let Some(&e) = out[u].get(*next) else {
                state[u] = 2;
                stack.pop();
                continue;
            };
            *next += 1;
            let v = edges[e].2;
            match state[v] {
                0 => {
                    state[v] = 1;
                    stack.push((v, 0));
                }
                1 => reversed[e] = true,
                _ => {}
            }
        }
    }
    reversed
}

/// Longest-path layers from the sources, with nodes that feed more than they consume pulled
/// right towards their successors, and empty layers removed.
fn assign_layers(n: usize, dag: &[(usize, usize)]) -> Vec<usize> {
    let mut succ = vec![Vec::new(); n];
    let mut indeg = vec![0usize; n];
    for &(u, v) in dag {
        succ[u].push(v);
        indeg[v] += 1;
    }
    let mut topo: Vec<usize> = (0..n).filter(|&v| indeg[v] == 0).collect();
    let mut remaining = indeg.clone();
    let mut head = 0;
    while head < topo.len() {
        let u = topo[head];
        head += 1;
        for &v in &succ[u] {
            remaining[v] -= 1;
            if remaining[v] == 0 {
                topo.push(v);
            }
        }
    }
    let mut layer = vec![0usize; n];
    for &u in &topo {
        for &v in &succ[u] {
            layer[v] = layer[v].max(layer[u] + 1);
        }
    }
    // Moving a node right by one shortens each out-edge and lengthens each in-edge by a layer.
    for &u in topo.iter().rev() {
        if succ[u].len() > indeg[u] {
            let limit = succ[u].iter().map(|&v| layer[v]).min().unwrap_or(0);
            layer[u] = layer[u].max(limit.saturating_sub(1));
        }
    }
    let mut used = vec![false; n];
    for &l in &layer {
        used[l] = true;
    }
    let mut remap = vec![0usize; n];
    let mut next = 0;
    for (l, &u) in used.iter().enumerate() {
        remap[l] = next;
        next += usize::from(u);
    }
    layer.iter().map(|&l| remap[l]).collect()
}

/// One kept input edge: its index in the input, whether it was reversed, and the nodes it passes
/// through in layer order.
struct Chain {
    edge: usize,
    reversed: bool,
    nodes: Vec<usize>,
}

/// The layered graph: real nodes first, then one dummy per layer crossed by a long edge.
struct Graph {
    real: usize,
    layer_count: usize,
    layer: Vec<usize>,
    height: Vec<f32>,
    up: Vec<Vec<usize>>,
    down: Vec<Vec<usize>>,
}

impl Graph {
    /// Builds the layered graph and, per DAG edge, the chain of nodes it runs through.
    fn build(
        nodes: &[NodeSpec],
        layer: &[usize],
        dag: &[(usize, usize)],
    ) -> (Graph, Vec<Vec<usize>>) {
        let n = nodes.len();
        let mut g = Graph {
            real: n,
            layer_count: layer.iter().max().map_or(0, |&l| l + 1),
            layer: layer.to_vec(),
            height: nodes.iter().map(|s| s.height.max(0.0)).collect(),
            up: vec![Vec::new(); n],
            down: vec![Vec::new(); n],
        };
        let chains = dag
            .iter()
            .map(|&(u, v)| {
                let mut chain = vec![u];
                for l in layer[u] + 1..layer[v] {
                    let d = g.layer.len();
                    g.layer.push(l);
                    g.height.push(0.0);
                    g.up.push(Vec::new());
                    g.down.push(Vec::new());
                    chain.push(d);
                }
                chain.push(v);
                for w in chain.windows(2) {
                    g.down[w[0]].push(w[1]);
                    g.up[w[1]].push(w[0]);
                }
                chain
            })
            .collect();
        (g, chains)
    }

    fn is_dummy(&self, v: usize) -> bool {
        v >= self.real
    }
}

/// Orders each layer: DFS preorder first, then barycenter sweeps, keeping the fewest crossings.
fn order_layers(g: &Graph) -> Vec<Vec<usize>> {
    let total = g.layer.len();
    let mut layers = vec![Vec::new(); g.layer_count];
    let mut seen = vec![false; total];
    let mut stack = Vec::new();
    for root in 0..g.real {
        stack.push(root);
        while let Some(v) = stack.pop() {
            if seen[v] {
                continue;
            }
            seen[v] = true;
            layers[g.layer[v]].push(v);
            stack.extend(g.down[v].iter().rev());
        }
    }
    let mut pos = vec![0usize; total];
    for layer in &layers {
        for (i, &v) in layer.iter().enumerate() {
            pos[v] = i;
        }
    }
    let mut best = layers.clone();
    let mut best_crossings = count_crossings(g, &layers, &pos);
    for sweep in 0..ORDER_SWEEPS {
        if best_crossings == 0 {
            break;
        }
        if sweep % 2 == 0 {
            for layer in layers.iter_mut().skip(1) {
                reorder(layer, &mut pos, &g.up);
            }
        } else {
            for layer in layers.iter_mut().rev().skip(1) {
                reorder(layer, &mut pos, &g.down);
            }
        }
        let crossings = count_crossings(g, &layers, &pos);
        if crossings < best_crossings {
            best_crossings = crossings;
            best.clone_from(&layers);
        }
    }
    best
}

/// Sorts one layer by the mean position of each node's neighbours in the adjacent layer.
///
/// A node without neighbours keys on its own position. The sort is stable, so ties keep the
/// previous order.
fn reorder(layer: &mut [usize], pos: &mut [usize], neighbours: &[Vec<usize>]) {
    let mut keyed: Vec<(f32, usize)> = layer
        .iter()
        .map(|&v| {
            let nb = &neighbours[v];
            let key = if nb.is_empty() {
                pos[v] as f32
            } else {
                nb.iter().map(|&w| pos[w] as f32).sum::<f32>() / nb.len() as f32
            };
            (key, v)
        })
        .collect();
    keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (i, (slot, &(_, v))) in layer.iter_mut().zip(&keyed).enumerate() {
        *slot = v;
        pos[v] = i;
    }
}

/// Counts edge crossings between every pair of adjacent layers.
fn count_crossings(g: &Graph, layers: &[Vec<usize>], pos: &[usize]) -> usize {
    let mut total = 0;
    let mut pairs = Vec::new();
    for l in 0..layers.len().saturating_sub(1) {
        pairs.clear();
        for &u in &layers[l] {
            pairs.extend(g.down[u].iter().map(|&w| (pos[u], pos[w])));
        }
        pairs.sort_unstable();
        // Fenwick tree over lower positions: each edge crosses the earlier edges that end below it.
        let size = layers[l + 1].len();
        let mut tree = vec![0usize; size + 1];
        for (seen, &(_, lower)) in pairs.iter().enumerate() {
            let mut at_or_above = 0;
            let mut i = lower + 1;
            while i > 0 {
                at_or_above += tree[i];
                i &= i - 1;
            }
            total += seen - at_or_above;
            let mut i = lower + 1;
            while i <= size {
                tree[i] += 1;
                i += i.isolate_lowest_one();
            }
        }
    }
    total
}

/// Vertical centres for every node, top of the drawing at 0.
fn place_vertically(g: &Graph, layers: &[Vec<usize>], gap: f32) -> Vec<f32> {
    let mut center = vec![0.0f32; g.layer.len()];
    for layer in layers {
        let mut y = 0.0;
        for &v in layer {
            center[v] = y + g.height[v] / 2.0;
            y += g.height[v] + gap;
        }
    }
    let none: Vec<Vec<usize>> = vec![Vec::new(); g.layer.len()];
    for _ in 0..PLACE_PASSES {
        for layer in layers.iter().skip(1) {
            align_layer(g, layer, &mut center, [&g.up, &none], gap);
        }
        for layer in layers.iter().rev().skip(1) {
            align_layer(g, layer, &mut center, [&g.down, &none], gap);
        }
    }
    for layer in layers {
        align_layer(g, layer, &mut center, [&g.up, &g.down], gap);
    }
    let top = layers
        .iter()
        .flatten()
        .map(|&v| center[v] - g.height[v] / 2.0)
        .fold(f32::INFINITY, f32::min);
    for c in &mut center {
        *c -= top;
    }
    center
}

/// Moves one layer's centres to the weighted least-squares fit of their neighbours' mean centre,
/// keeping the order and at least `gap` between boxes.
fn align_layer(
    g: &Graph,
    layer: &[usize],
    center: &mut [f32],
    sides: [&Vec<Vec<usize>>; 2],
    gap: f32,
) {
    let mut targets = Vec::with_capacity(layer.len());
    let mut offset = 0.0;
    for (i, &v) in layer.iter().enumerate() {
        if i > 0 {
            offset += (g.height[layer[i - 1]] + g.height[v]) / 2.0 + gap;
        }
        let nb = sides.iter().flat_map(|s| s[v].iter());
        let count = nb.clone().count();
        let (want, weight) = if count == 0 {
            (center[v], LONE_WEIGHT)
        } else {
            let mean = nb.map(|&w| center[w]).sum::<f32>() / count as f32;
            (mean, if g.is_dummy(v) { DUMMY_WEIGHT } else { 1.0 })
        };
        // Subtracting the minimum offset turns separation into plain monotonicity.
        targets.push((want - offset, weight, offset));
    }
    // Pool adjacent violators: merge blocks while a block's mean exceeds its successor's.
    let mut blocks: Vec<(f32, f32, usize)> = Vec::new();
    for &(t, w, _) in &targets {
        blocks.push((t * w, w, 1));
        while let [.., (s0, w0, n0), (s1, w1, n1)] = blocks[..] {
            if s0 / w0 <= s1 / w1 {
                break;
            }
            blocks.truncate(blocks.len() - 2);
            blocks.push((s0 + s1, w0 + w1, n0 + n1));
        }
    }
    let mut i = 0;
    for (sum, weight, len) in blocks {
        for _ in 0..len {
            center[layer[i]] = sum / weight + targets[i].2;
            i += 1;
        }
    }
}

/// Turns layer columns and vertical centres into placements, routes and the bounding size.
fn assemble(
    nodes: &[NodeSpec],
    edges: &[(u64, u64)],
    spacing: Spacing,
    g: &Graph,
    layers: &[Vec<usize>],
    center: &[f32],
    chains: &[Chain],
) -> Layout {
    let widest: Vec<f32> = layers
        .iter()
        .map(|l| {
            l.iter()
                .filter(|&&v| !g.is_dummy(v))
                .map(|&v| nodes[v].width.max(0.0))
                .fold(0.0, f32::max)
        })
        .collect();
    let mut column = Vec::with_capacity(layers.len());
    let mut x = 0.0;
    for &w in &widest {
        column.push(x);
        x += w + spacing.layer_gap;
    }
    let width = x - spacing.layer_gap;
    let mut placements: Vec<Placement> = nodes
        .iter()
        .enumerate()
        .map(|(v, s)| Placement {
            x: column[g.layer[v]],
            y: center[v] - g.height[v] / 2.0,
            width: s.width.max(0.0),
            height: g.height[v],
            layer: g.layer[v],
            order: 0,
        })
        .collect();
    for layer in layers {
        for (order, &v) in layer.iter().filter(|&&v| !g.is_dummy(v)).enumerate() {
            placements[v].order = order;
        }
    }
    let routes = chains
        .iter()
        .map(|c| {
            let chain = &c.nodes;
            let (s, t) = (chain[0], chain[chain.len() - 1]);
            let mut points = vec![(column[g.layer[s]] + placements[s].width, center[s])];
            for &d in &chain[1..chain.len() - 1] {
                let l = g.layer[d];
                points.push((column[l], center[d]));
                if widest[l] > 0.0 {
                    points.push((column[l] + widest[l], center[d]));
                }
            }
            points.push((column[g.layer[t]], center[t]));
            if c.reversed {
                points.reverse();
            }
            Route {
                from: edges[c.edge].0,
                to: edges[c.edge].1,
                points,
                reversed: c.reversed,
            }
        })
        .collect();
    let height = (0..g.layer.len())
        .map(|v| center[v] + g.height[v] / 2.0)
        .fold(0.0, f32::max);
    Layout {
        nodes: placements,
        routes,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxes(ids: &[u64]) -> Vec<NodeSpec> {
        ids.iter()
            .map(|&id| NodeSpec {
                id,
                width: 100.0 + (id % 3) as f32 * 20.0,
                height: 30.0 + (id % 4) as f32 * 10.0,
            })
            .collect()
    }

    /// A random graph with forward edges, back edges (cycles), self-loops and duplicates.
    fn random_graph(n: u64, m: usize, seed: u64) -> (Vec<NodeSpec>, Vec<(u64, u64)>) {
        let mut s = seed;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let nodes = boxes(&(0..n).map(|i| i * 7 + 3).collect::<Vec<_>>());
        let edges = (0..m)
            .map(|_| {
                let a = next() % n;
                let b = if next() % 8 == 0 {
                    next() % n
                } else {
                    (a + 1 + next() % 6).min(n - 1)
                };
                (a * 7 + 3, b * 7 + 3)
            })
            .collect();
        (nodes, edges)
    }

    fn assert_well_formed(layout: &Layout, gap: f32) {
        for (i, a) in layout.nodes.iter().enumerate() {
            assert!(a.x >= 0.0 && a.y >= 0.0, "node {i} at negative coordinates");
            assert!(a.x + a.width <= layout.width + 1e-3);
            assert!(a.y + a.height <= layout.height + 1e-3);
            for b in &layout.nodes[i + 1..] {
                let apart_x = a.x + a.width <= b.x || b.x + b.width <= a.x;
                let apart_y =
                    a.y + a.height + gap <= b.y + 1e-3 || b.y + b.height + gap <= a.y + 1e-3;
                assert!(apart_x || apart_y, "{a:?} overlaps {b:?}");
            }
        }
        for r in &layout.routes {
            let points: Vec<_> = if r.reversed {
                r.points.iter().rev().copied().collect()
            } else {
                r.points.clone()
            };
            for w in points.windows(2) {
                assert!(w[0].0 < w[1].0, "route {r:?} is not strictly left to right");
            }
        }
    }

    #[test]
    fn random_graphs_are_well_formed() {
        for seed in 1..20 {
            let (nodes, edges) = random_graph(60, 140, seed * 0x9E37_79B9);
            let spacing = Spacing {
                layer_gap: 30.0,
                node_gap: 10.0,
            };
            let l = Layout::compute(&nodes, &edges, spacing);
            assert_eq!(l.nodes.len(), nodes.len());
            assert_well_formed(&l, 10.0);
            let left = l.nodes.iter().map(|p| p.x).fold(f32::INFINITY, f32::min);
            let node_top = l.nodes.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
            let route_top = l.routes.iter().flat_map(|r| r.points.iter().map(|p| p.1));
            let top = route_top.fold(node_top, f32::min);
            assert_eq!(left, 0.0);
            assert!(top.abs() < 1e-3, "drawing starts at y = {top}");
        }
    }

    #[test]
    fn output_is_deterministic() {
        let (nodes, edges) = random_graph(200, 400, 42);
        let a = Layout::compute(&nodes, &edges, Spacing::default());
        // A second run on another thread gets fresh std hasher seeds, were any used.
        let b = std::thread::spawn(move || Layout::compute(&nodes, &edges, Spacing::default()))
            .join()
            .unwrap();
        assert_eq!(a, b);
    }
}
