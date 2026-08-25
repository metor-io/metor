//! Route emission: where each edge attaches to its nodes and which interior
//! waypoints it bends through.
//!
//! Forward edges thread through the wire channels their virtual nodes
//! reserved. Backward edges detour through a horizontal channel above or
//! below the rows they span, one parallel lane per edge, instead of slicing
//! across the cards. Same-layer edges take a short bracket beside their
//! layer. Anchors fan out along a node's side instead of piling onto its
//! center: per side, endpoints are sorted by where their wire heads next, so
//! a pair of edges that would cross right at the boundary untwists before it
//! leaves the card.

use std::collections::BTreeMap;

use super::coords::Geometry;
use super::order::{Arena, EdgeClass};
use super::{EdgeRoute, LayoutEdge, PinAnchor};

/// Minimum distance from a node corner to the first fanned anchor.
const EDGE_INSET: f32 = 10.0;
/// Cross-axis clearance between a spanned row extent and the first detour
/// channel.
const CHANNEL_GAP: f32 = 24.0;
/// Cross-axis pitch between parallel detour lanes.
const LANE_PITCH: f32 = 12.0;
/// Flow-axis clearance for a same-layer bracket and for detour elbows.
const ELBOW_GAP: f32 = 32.0;

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
    let half = |v: usize| -> f32 {
        match arena.orig[v] {
            Some(i) => sizes[i].1 / 2.0,
            None => 0.0,
        }
    };

    // Occupied cross extent per layer, endpoints included, so detours know
    // what to clear.
    let layer_count = arena.by_layer.len();
    let mut layer_top = vec![f32::INFINITY; layer_count];
    let mut layer_bot = vec![f32::NEG_INFINITY; layer_count];
    for (l, members) in arena.by_layer.iter().enumerate() {
        for &v in members {
            layer_top[l] = layer_top[l].min(geo.cross[v] - half(v));
            layer_bot[l] = layer_bot[l].max(geo.cross[v] + half(v));
        }
    }

    let mut routes: Vec<EdgeRoute> = vec![EdgeRoute::default(); edges.len()];

    // Backward edges claim parallel lanes per side, widest span innermost so
    // nested feedback wires never overprint. Everything here is index- and
    // total_cmp-ordered, so lanes are deterministic.
    let mut back_edges: Vec<(usize, usize)> = edges
        .iter()
        .enumerate()
        .filter(|&(ei, _)| arena.class[ei] == EdgeClass::Back)
        .map(|(ei, e)| {
            let span = arena.layer[e.from] - arena.layer[e.to];
            (ei, span)
        })
        .collect();
    back_edges.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut lanes_above = 0usize;
    let mut lanes_below = 0usize;

    for (ei, e) in edges.iter().enumerate() {
        match arena.class[ei] {
            EdgeClass::Forward => {
                for &d in &arena.dummies[ei] {
                    let l = arena.layer[d];
                    routes[ei]
                        .waypoints
                        .push((geo.layer_pos[l] + geo.layer_extent[l] / 2.0, geo.cross[d]));
                }
            }
            EdgeClass::Flat => {
                let l = arena.layer[e.from];
                let bracket = geo.layer_pos[l] + geo.layer_extent[l] + ELBOW_GAP;
                let mid = (geo.cross[e.from] + geo.cross[e.to]) / 2.0;
                routes[ei].waypoints.push((bracket, mid));
            }
            EdgeClass::Back => {}
        }
    }

    for &(ei, _) in &back_edges {
        let e = &edges[ei];
        let (lu, lv) = (arena.layer[e.from], arena.layer[e.to]);
        let mut span_top = f32::INFINITY;
        let mut span_bot = f32::NEG_INFINITY;
        for l in lv..=lu {
            span_top = span_top.min(layer_top[l]);
            span_bot = span_bot.max(layer_bot[l]);
        }
        let mean = (geo.cross[e.from] + geo.cross[e.to]) / 2.0;
        let above = mean - span_top <= span_bot - mean;
        let channel = if above {
            let lane = lanes_above;
            lanes_above += 1;
            span_top - (CHANNEL_GAP + lane as f32 * LANE_PITCH)
        } else {
            let lane = lanes_below;
            lanes_below += 1;
            span_bot + CHANNEL_GAP + lane as f32 * LANE_PITCH
        };
        let exit = positions[e.from].0 + sizes[e.from].0 + ELBOW_GAP;
        let entry = positions[e.to].0 - ELBOW_GAP;
        routes[ei].waypoints.push((exit, channel));
        routes[ei].waypoints.push((entry, channel));
    }

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
        // A same-layer wire brackets around the outgoing side, so it enters
        // its target from the right as well.
        let target_side = match arena.class[ei] {
            EdgeClass::Flat => Side::Out,
            _ => Side::In,
        };
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
        let PinAnchor::Auto = e.from_pin;
        fans.entry((e.from, Side::Out)).or_default().push(Endpoint {
            edge: ei,
            is_source: true,
            heading: heading_out,
        });
        let PinAnchor::Auto = e.to_pin;
        fans.entry((e.to, target_side)).or_default().push(Endpoint {
            edge: ei,
            is_source: false,
            heading: heading_in,
        });
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
