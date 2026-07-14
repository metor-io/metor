//! Coordinate assignment: real-size packing plus a straightening pass.
//!
//! Layers get a flow-axis extent from their widest member; within a layer,
//! nodes stack by their real cross extent. The straightening pass is
//! Sugiyama's priority method rather than Brandes–Köpf: a fraction of the
//! code, deterministic, and indistinguishable at the tens-of-nodes scale
//! these panes draw. Virtual nodes get top priority so a long edge pulls
//! itself straight through its waypoints.

use super::LayoutOptions;
use super::order::Arena;

/// Nominal cross extent a virtual node reserves, so a wire channel stays
/// open between real cards.
const DUMMY_EXTENT: f32 = 8.0;

pub(super) struct Geometry {
    /// Flow-axis start of each layer's extent.
    pub layer_pos: Vec<f32>,
    /// Flow-axis extent (widest member) of each layer.
    pub layer_extent: Vec<f32>,
    /// Cross-axis center per vnode.
    pub cross: Vec<f32>,
}

pub(super) fn assign(arena: &Arena, sizes: &[(f32, f32)], opts: &LayoutOptions) -> Geometry {
    let half = |v: usize| -> f32 {
        match arena.orig[v] {
            Some(i) => sizes[i].1 / 2.0,
            None => DUMMY_EXTENT / 2.0,
        }
    };

    let mut layer_extent = vec![0f32; arena.by_layer.len()];
    for (l, members) in arena.by_layer.iter().enumerate() {
        for &v in members {
            if let Some(i) = arena.orig[v] {
                layer_extent[l] = layer_extent[l].max(sizes[i].0);
            }
        }
    }
    let mut layer_pos = Vec::with_capacity(layer_extent.len());
    let mut flow = opts.margin;
    for &extent in &layer_extent {
        layer_pos.push(flow);
        flow += extent + opts.layer_gap;
    }

    let mut cross = vec![0f32; arena.orig.len()];
    for members in &arena.by_layer {
        let mut cursor = opts.margin;
        for &v in members {
            cross[v] = cursor + half(v);
            cursor += half(v) * 2.0 + opts.node_gap;
        }
    }

    // Straightening sweeps: down (align to predecessors), up (successors),
    // down again to settle.
    let max_layer = arena.by_layer.len() - 1;
    for l in 1..=max_layer {
        sweep_layer(arena, &arena.pred, l, opts.node_gap, &half, &mut cross);
    }
    for l in (0..max_layer).rev() {
        sweep_layer(arena, &arena.succ, l, opts.node_gap, &half, &mut cross);
    }
    for l in 1..=max_layer {
        sweep_layer(arena, &arena.pred, l, opts.node_gap, &half, &mut cross);
    }

    // Re-normalize so the topmost extent sits at the margin.
    let min = (0..arena.orig.len())
        .map(|v| cross[v] - half(v))
        .fold(f32::INFINITY, f32::min);
    if min.is_finite() {
        let shift = opts.margin - min;
        for c in &mut cross {
            *c += shift;
        }
    }

    Geometry {
        layer_pos,
        layer_extent,
        cross,
    }
}

/// One priority-method pass over layer `l`: in descending priority, move each
/// vnode's cross center toward the mean of its `neighbors` in the fixed
/// adjacent layer. A move is clamped so it cannot encroach on a sibling of
/// equal or higher priority, and pushes any lower-priority siblings in the
/// way just far enough to keep the minimum gap.
fn sweep_layer(
    arena: &Arena,
    neighbors: &[Vec<usize>],
    l: usize,
    gap: f32,
    half: &dyn Fn(usize) -> f32,
    cross: &mut [f32],
) {
    let members = &arena.by_layer[l];
    // Virtual nodes outrank everything so long edges pull straight; real
    // nodes rank by how many wires anchor them in the sweep direction.
    let prio: Vec<usize> = members
        .iter()
        .map(|&v| match arena.orig[v] {
            Some(_) => neighbors[v].len(),
            None => usize::MAX,
        })
        .collect();
    let min_sep = |a: usize, b: usize| half(a) + half(b) + gap;

    let mut by_prio: Vec<usize> = (0..members.len()).collect();
    by_prio.sort_by(|&a, &b| prio[b].cmp(&prio[a]).then(a.cmp(&b)));

    for &i in &by_prio {
        let v = members[i];
        let nbrs = &neighbors[v];
        if nbrs.is_empty() {
            continue;
        }
        let desired = nbrs.iter().map(|&m| cross[m]).sum::<f32>() / nbrs.len() as f32;

        // The nearest sibling of equal-or-higher priority on each side is
        // immovable; everything between is pushable, so the bound is that
        // sibling's position plus the stacked minimum separations.
        let mut lo = f32::NEG_INFINITY;
        let mut acc = 0.0;
        for j in (0..i).rev() {
            acc += min_sep(members[j], members[j + 1]);
            if prio[j] >= prio[i] {
                lo = cross[members[j]] + acc;
                break;
            }
        }
        let mut hi = f32::INFINITY;
        acc = 0.0;
        for j in i + 1..members.len() {
            acc += min_sep(members[j - 1], members[j]);
            if prio[j] >= prio[i] {
                hi = cross[members[j]] - acc;
                break;
            }
        }
        if lo > hi {
            continue;
        }
        cross[v] = desired.clamp(lo, hi);

        for j in (0..i).rev() {
            let (a, b) = (members[j], members[j + 1]);
            let limit = cross[b] - min_sep(a, b);
            if cross[a] > limit {
                cross[a] = limit;
            } else {
                break;
            }
        }
        for j in i + 1..members.len() {
            let (a, b) = (members[j - 1], members[j]);
            let limit = cross[a] + min_sep(a, b);
            if cross[b] < limit {
                cross[b] = limit;
            } else {
                break;
            }
        }
    }
}
