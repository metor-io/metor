//! Route emission: where each edge attaches to its nodes and which interior
//! waypoints it bends through.
//!
//! Anchors fan out along a node's side instead of piling onto its center:
//! per side, endpoints are sorted by where their wire heads next (the first
//! waypoint, or the far node), so a pair of edges that would cross right at
//! the boundary untwists before it leaves the card.

use std::collections::BTreeMap;

use smallvec::SmallVec;

use super::coords::Geometry;
use super::order::Arena;
use super::{EdgeRoute, LayoutEdge, PinAnchor};

/// Minimum distance from a node corner to the first fanned anchor.
const EDGE_INSET: f32 = 10.0;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Side {
    /// The layer-facing incoming side (left, in normalized orientation).
    In,
    /// The outgoing side (right).
    Out,
}

struct Endpoint {
    edge: usize,
    is_source: bool,
    /// Cross position of the wire's next stop, used to order the fan.
    heading: f32,
}

pub(super) fn routes(
    arena: &Arena,
    geo: &Geometry,
    sizes: &[(f32, f32)],
    positions: &[(f32, f32)],
    edges: &[LayoutEdge],
) -> Vec<EdgeRoute> {
    let mut routes: Vec<EdgeRoute> = edges
        .iter()
        .enumerate()
        .map(|(ei, _)| {
            let mut waypoints = SmallVec::new();
            for &d in &arena.dummies[ei] {
                let l = arena.layer[d];
                waypoints.push((geo.layer_pos[l] + geo.layer_extent[l] / 2.0, geo.cross[d]));
            }
            EdgeRoute {
                waypoints,
                ..EdgeRoute::default()
            }
        })
        .collect();

    let anchor = |node: usize, side: Side, off: f32| -> (f32, f32) {
        let (x, y) = positions[node];
        let flow = match side {
            Side::In => x,
            Side::Out => x + sizes[node].0,
        };
        (flow, y + off)
    };

    let mut fans: BTreeMap<(usize, Side), Vec<Endpoint>> = BTreeMap::new();
    for (ei, e) in edges.iter().enumerate() {
        let heading_out = routes[ei]
            .waypoints
            .first()
            .map(|w| w.1)
            .unwrap_or(geo.cross[e.to]);
        let heading_in = routes[ei]
            .waypoints
            .last()
            .map(|w| w.1)
            .unwrap_or(geo.cross[e.from]);
        match e.from_pin {
            PinAnchor::Auto => fans.entry((e.from, Side::Out)).or_default().push(Endpoint {
                edge: ei,
                is_source: true,
                heading: heading_out,
            }),
            PinAnchor::Offset(off) => {
                routes[ei].source = anchor(e.from, Side::Out, off.clamp(0.0, sizes[e.from].1));
            }
        }
        match e.to_pin {
            PinAnchor::Auto => fans.entry((e.to, Side::In)).or_default().push(Endpoint {
                edge: ei,
                is_source: false,
                heading: heading_in,
            }),
            PinAnchor::Offset(off) => {
                routes[ei].target = anchor(e.to, Side::In, off.clamp(0.0, sizes[e.to].1));
            }
        }
    }

    for ((node, side), mut endpoints) in fans {
        endpoints.sort_by(|a, b| {
            a.heading
                .total_cmp(&b.heading)
                .then(a.edge.cmp(&b.edge))
                .then(a.is_source.cmp(&b.is_source))
        });
        let h = sizes[node].1;
        let k = endpoints.len() as f32;
        for (i, ep) in endpoints.iter().enumerate() {
            let off = (h * (i as f32 + 1.0) / (k + 1.0))
                .clamp(EDGE_INSET.min(h / 2.0), (h - EDGE_INSET).max(h / 2.0));
            let p = anchor(node, side, off);
            if ep.is_source {
                routes[ep.edge].source = p;
            } else {
                routes[ep.edge].target = p;
            }
        }
    }

    routes
}
