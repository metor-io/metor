use super::*;

fn node(w: f32, h: f32) -> LayoutNode {
    LayoutNode { size: (w, h) }
}

fn edge(from: usize, to: usize) -> LayoutEdge {
    LayoutEdge {
        from,
        to,
        from_pin: PinAnchor::Auto,
        to_pin: PinAnchor::Auto,
        ranked: true,
    }
}

fn compute_with(
    nodes: &[LayoutNode],
    edges: &[LayoutEdge],
    options: LayoutOptions,
) -> LayoutOutput {
    compute(&LayoutInput {
        nodes,
        edges,
        options,
    })
}

fn lr(nodes: &[LayoutNode], edges: &[LayoutEdge]) -> LayoutOutput {
    compute_with(nodes, edges, LayoutOptions::default())
}

fn rect(out: &LayoutOutput, nodes: &[LayoutNode], i: usize) -> (f32, f32, f32, f32) {
    let (x, y) = out.positions[i];
    (x, y, nodes[i].size.0, nodes[i].size.1)
}

fn overlaps(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

/// A chain occupies consecutive layers, advancing along the flow axis.
#[test]
fn chain_layers_advance() {
    let nodes = vec![node(188.0, 62.0); 3];
    let edges = vec![edge(0, 1), edge(1, 2)];
    let out = lr(&nodes, &edges);
    assert_eq!(out.layers, vec![0, 1, 2]);
    assert!(out.positions[0].0 < out.positions[1].0);
    assert!(out.positions[1].0 < out.positions[2].0);
}

/// A source feeding only a deep consumer moves next to it instead of
/// stretching a long edge from layer 0.
#[test]
fn source_tightens_toward_consumer() {
    // 0 → 1 → 2, plus source 3 → 2.
    let nodes = vec![node(188.0, 62.0); 4];
    let edges = vec![edge(0, 1), edge(1, 2), edge(3, 2)];
    let out = lr(&nodes, &edges);
    assert_eq!(out.layers[3], 1);
}

/// A span-3 edge gets one waypoint per intermediate layer.
#[test]
fn long_edge_routes_through_waypoints() {
    // 0 → 1 → 2 → 3 and a long edge 0 → 3.
    let nodes = vec![node(188.0, 62.0); 4];
    let edges = vec![edge(0, 1), edge(1, 2), edge(2, 3), edge(0, 3)];
    let out = lr(&nodes, &edges);
    assert_eq!(out.routes[3].waypoints.len(), 2);
    // Waypoints advance monotonically along the flow axis, between the nodes.
    let wps = &out.routes[3].waypoints;
    assert!(out.positions[0].0 < wps[0].0);
    assert!(wps[0].0 < wps[1].0);
    assert!(wps[1].0 < out.positions[3].0);
}

/// Real heights stack without overlap even when they differ per node.
#[test]
fn mixed_heights_never_overlap() {
    let nodes = vec![
        node(188.0, 62.0),
        node(188.0, 84.0),
        node(188.0, 44.0),
        node(188.0, 84.0),
        node(188.0, 62.0),
    ];
    // All in one layer feeding one sink: 5 nodes stacked in layer 0.
    let edges: Vec<LayoutEdge> = Vec::new();
    let out = lr(&nodes, &edges);
    for i in 0..nodes.len() {
        for j in i + 1..nodes.len() {
            let (a, b) = (rect(&out, &nodes, i), rect(&out, &nodes, j));
            assert!(!overlaps(a, b), "nodes {i} and {j} overlap: {a:?} vs {b:?}");
        }
    }
}

/// Crossing reduction untwists an X: if 0→3 and 1→2 start crossed, the
/// barycenter sweeps reorder one layer so the wires run parallel.
#[test]
fn crossing_reduction_untwists() {
    let nodes = vec![node(188.0, 62.0); 4];
    let edges = vec![edge(0, 3), edge(1, 2)];
    let out = lr(&nodes, &edges);
    // Sources 0,1 sit in layer 0; targets 2,3 in layer 1. Parallel wires mean
    // the cross order of (0,1) matches the cross order of (3,2).
    let above = |a: usize, b: usize| out.positions[a].1 < out.positions[b].1;
    assert_eq!(above(0, 1), above(3, 2));
}

/// A pass-through chain is straightened into a collinear run.
#[test]
fn chain_straightens_collinear() {
    let nodes = vec![node(188.0, 62.0); 3];
    let edges = vec![edge(0, 1), edge(1, 2)];
    let out = lr(&nodes, &edges);
    let mid = |i: usize| out.positions[i].1 + nodes[i].size.1 / 2.0;
    assert!((mid(0) - mid(1)).abs() < 1.0);
    assert!((mid(1) - mid(2)).abs() < 1.0);
}

/// Multiple edges leaving one node fan out to distinct anchors, ordered by
/// where each wire heads.
#[test]
fn anchors_fan_out_in_heading_order() {
    // 0 feeds 1, 2, 3 (stacked in the next layer).
    let nodes = vec![node(188.0, 62.0); 4];
    let edges = vec![edge(0, 1), edge(0, 2), edge(0, 3)];
    let out = lr(&nodes, &edges);
    let sources: Vec<f32> = (0..3).map(|i| out.routes[i].source.1).collect();
    assert!(sources[0] != sources[1] && sources[1] != sources[2]);
    // Anchor order matches target order.
    let mut expect: Vec<(f32, usize)> = (0..3).map(|i| (out.positions[edges[i].to].1, i)).collect();
    expect.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut anchors: Vec<(f32, usize)> = (0..3).map(|i| (out.routes[i].source.1, i)).collect();
    anchors.sort_by(|a, b| a.0.total_cmp(&b.0));
    let expect_order: Vec<usize> = expect.iter().map(|&(_, i)| i).collect();
    let anchor_order: Vec<usize> = anchors.iter().map(|&(_, i)| i).collect();
    assert_eq!(expect_order, anchor_order);
}

/// Offset pins are honored verbatim rather than fanned.
fn unranked(from: usize, to: usize) -> LayoutEdge {
    LayoutEdge {
        ranked: false,
        ..edge(from, to)
    }
}

/// A feedback edge detours through a channel clear of every card: both
/// elbows share a cross position that no node's extent contains.
#[test]
fn back_edge_detours_around_rows() {
    let nodes = vec![node(188.0, 62.0); 3];
    let edges = vec![edge(0, 1), edge(1, 2), unranked(2, 0)];
    let out = lr(&nodes, &edges);
    let wps = &out.routes[2].waypoints;
    assert_eq!(wps.len(), 2);
    assert_eq!(wps[0].1, wps[1].1);
    let channel = wps[0].1;
    for i in 0..nodes.len() {
        let (_, y, _, h) = rect(&out, &nodes, i);
        assert!(
            channel < y || channel > y + h,
            "channel {channel} cuts through node {i} ({y}..{})",
            y + h
        );
    }
}

/// Two feedback edges on the same side take parallel lanes instead of
/// overprinting.
#[test]
fn back_edges_take_separate_lanes() {
    let nodes = vec![node(188.0, 62.0); 4];
    let edges = vec![
        edge(0, 1),
        edge(1, 2),
        edge(2, 3),
        unranked(3, 0),
        unranked(2, 0),
    ];
    let out = lr(&nodes, &edges);
    let (a, b) = (&out.routes[3].waypoints, &out.routes[4].waypoints);
    assert_eq!(a.len(), 2);
    assert_eq!(b.len(), 2);
    assert!(a[0].1 != b[0].1, "feedback lanes overlap at {}", a[0].1);
}

/// A same-layer wire brackets beside its layer and enters the target from
/// the outgoing side.
#[test]
fn flat_edge_brackets_beside_layer() {
    let nodes = vec![node(188.0, 62.0); 2];
    let edges = vec![unranked(0, 1)];
    let out = lr(&nodes, &edges);
    assert_eq!(out.layers, vec![0, 0]);
    let wps = &out.routes[0].waypoints;
    assert_eq!(wps.len(), 1);
    assert!(wps[0].0 > out.positions[0].0 + 188.0);
    assert_eq!(out.routes[0].target.0, out.positions[1].0 + 188.0);
}

/// Identical input yields bit-identical output.
#[test]
fn deterministic() {
    let nodes = vec![node(188.0, 62.0); 6];
    let edges = vec![
        edge(0, 2),
        edge(1, 2),
        edge(2, 3),
        edge(2, 4),
        edge(4, 5),
        edge(0, 5),
    ];
    let a = lr(&nodes, &edges);
    let b = lr(&nodes, &edges);
    assert_eq!(a.positions, b.positions);
    assert_eq!(a.layers, b.layers);
    assert_eq!(a.routes, b.routes);
}

/// Top→bottom output is exactly the transpose of left→right run on
/// transposed node sizes.
#[test]
fn top_bottom_transposes() {
    let nodes = vec![node(188.0, 62.0), node(188.0, 84.0), node(188.0, 44.0)];
    let swapped: Vec<LayoutNode> = nodes.iter().map(|n| node(n.size.1, n.size.0)).collect();
    let edges = vec![edge(0, 1), edge(1, 2), edge(0, 2)];
    let lr_out = lr(&swapped, &edges);
    let tb_out = compute_with(
        &nodes,
        &edges,
        LayoutOptions {
            direction: Direction::TopBottom,
            ..LayoutOptions::default()
        },
    );
    for i in 0..nodes.len() {
        assert_eq!(
            (lr_out.positions[i].1, lr_out.positions[i].0),
            tb_out.positions[i]
        );
    }
    for (a, b) in lr_out.routes.iter().zip(&tb_out.routes) {
        assert_eq!((a.source.1, a.source.0), b.source);
        assert_eq!((a.target.1, a.target.0), b.target);
        for (wa, wb) in a.waypoints.iter().zip(&b.waypoints) {
            assert_eq!((wa.1, wa.0), *wb);
        }
    }
}
