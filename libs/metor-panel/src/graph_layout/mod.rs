//! Deterministic layered ("Sugiyama") layout shared by graph panes.
//!
//! The system graph and the node editor both need to arrange a directed
//! dataflow graph so that flow reads in one direction and wires stay legible.
//! This engine is the shared geometry core: callers describe nodes (real
//! sizes), pins, and edges; it returns node positions plus a drawable route
//! per edge (boundary anchors and interior waypoints). It is pure `f32`
//! geometry — no gpui types — so every stage is unit-testable, and the whole
//! computation is a deterministic function of its input.
//!
//! The pipeline is the classic layered framework: layer assignment over the
//! acyclic skeleton ([`rank`]), crossing reduction with virtual nodes standing
//! in for long edges ([`order`]), coordinate assignment from real node sizes
//! with a straightening pass ([`coords`]), and route emission with per-side
//! anchor fan-out ([`route`]). Everything internal runs in left→right
//! orientation; [`Direction::TopBottom`] transposes sizes on the way in and
//! coordinates on the way out, so the algorithms are written once,
//! direction-free.

mod coords;
mod order;
mod rank;
mod route;

#[cfg(test)]
mod tests;

use smallvec::SmallVec;

/// Index into [`LayoutInput::nodes`].
pub type NodeIx = usize;

/// Flow direction of the layered layout.
#[derive(facet::Facet, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Direction {
    #[default]
    LeftRight,
    TopBottom,
}

impl Direction {
    pub fn cycle(self) -> Self {
        match self {
            Self::LeftRight => Self::TopBottom,
            Self::TopBottom => Self::LeftRight,
        }
    }
}

/// Where an edge endpoint attaches to its node.
#[derive(Clone, Copy, Debug)]
pub enum PinAnchor {
    /// The engine fans endpoints out along the node's side, ordered so that
    /// locally-crossing pairs untwist at the boundary.
    Auto,
    /// Fixed offset along the node's cross axis from its top (left in
    /// [`Direction::TopBottom`]) edge — the caller owns pin geometry.
    Offset(f32),
}

/// One node to lay out; the engine only needs its footprint.
#[derive(Clone, Copy, Debug)]
pub struct LayoutNode {
    /// `(width, height)` in graph units — the real card size.
    pub size: (f32, f32),
}

/// One directed edge between node pins.
#[derive(Clone, Copy, Debug)]
pub struct LayoutEdge {
    pub from: NodeIx,
    pub to: NodeIx,
    pub from_pin: PinAnchor,
    pub to_pin: PinAnchor,
    /// Participates in layer assignment. The ranked subset must be the
    /// caller's acyclic skeleton (e.g. non-delayed frame edges); everything
    /// else — feedback edges, message channels — is placed by whatever
    /// layers that skeleton produces and routed by where it lands: forward
    /// wires thread the layer channels, backward wires detour around the
    /// rows they span.
    pub ranked: bool,
}

/// Spacing knobs, all in graph units.
#[derive(Clone, Copy, Debug)]
pub struct LayoutOptions {
    pub direction: Direction,
    /// Gap between adjacent layer extents along the flow axis.
    pub layer_gap: f32,
    /// Minimum cross-axis gap between siblings within a layer.
    pub node_gap: f32,
    /// Margin from the origin to the first node.
    pub margin: f32,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        Self {
            direction: Direction::LeftRight,
            layer_gap: 104.0,
            node_gap: 24.0,
            margin: 32.0,
        }
    }
}

/// Everything the engine needs, borrowed from the caller.
pub struct LayoutInput<'a> {
    pub nodes: &'a [LayoutNode],
    pub edges: &'a [LayoutEdge],
    /// Tie-break rank per node (the caller's declaration order), so ordering
    /// ties resolve deterministically without the engine knowing about ids.
    pub tie_break: &'a [usize],
    pub options: LayoutOptions,
}

/// A drawable route: explicit boundary anchors plus interior waypoints (a
/// long edge's bend points, or a detour's elbows), all in graph space. The
/// anchors are explicit because sides are not always out-right/in-left — a
/// feedback edge may enter and leave on the same side.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EdgeRoute {
    pub source: (f32, f32),
    pub waypoints: SmallVec<[(f32, f32); 4]>,
    pub target: (f32, f32),
}

/// The computed layout, indexed like the input slices.
#[derive(Debug)]
pub struct LayoutOutput {
    /// Node top-left positions.
    pub positions: Vec<(f32, f32)>,
    /// Layer index per node.
    pub layers: Vec<usize>,
    /// Route per edge.
    pub routes: Vec<EdgeRoute>,
}

/// Lay out the graph. Deterministic: identical input yields bit-identical
/// output.
pub fn compute(input: &LayoutInput) -> LayoutOutput {
    let n = input.nodes.len();
    debug_assert_eq!(input.tie_break.len(), n);
    let transposed = input.options.direction == Direction::TopBottom;

    // Normalized (left→right) sizes; a transposed layout swaps them back at
    // the end, which also makes `PinAnchor::Offset` measure along whichever
    // axis is "down the node's side" for the chosen direction.
    let sizes: Vec<(f32, f32)> = input
        .nodes
        .iter()
        .map(|node| {
            if transposed {
                (node.size.1, node.size.0)
            } else {
                node.size
            }
        })
        .collect();

    let layers = rank::assign_layers(n, input.edges);
    let arena = order::Arena::build(n, input.edges, &layers, input.tie_break);
    let geo = coords::assign(&arena, &sizes, &input.options);

    let mut positions: Vec<(f32, f32)> = (0..n)
        .map(|i| {
            let l = layers[i];
            let (w, h) = sizes[i];
            let flow = geo.layer_pos[l] + (geo.layer_extent[l] - w) / 2.0;
            (flow, geo.cross[i] - h / 2.0)
        })
        .collect();
    let mut routes = route::routes(&arena, &geo, &sizes, &positions, input.edges);

    if transposed {
        let swap = |p: &mut (f32, f32)| *p = (p.1, p.0);
        for p in &mut positions {
            swap(p);
        }
        for r in &mut routes {
            swap(&mut r.source);
            swap(&mut r.target);
            for w in &mut r.waypoints {
                swap(w);
            }
        }
    }

    LayoutOutput {
        positions,
        layers,
        routes,
    }
}
