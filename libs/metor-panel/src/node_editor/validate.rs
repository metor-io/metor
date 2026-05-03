//! Pure validation: socket-type compatibility, cycle prevention, and clock
//! coloring. Used by the canvas to accept/reject connection drops and to
//! tint edges after a build.

use std::collections::{HashSet, VecDeque};

use crate::dynamic::BuildError;
use crate::node_editor::graph::{BuildState, EdgeEntry, FlowId, NodeGraph};
use crate::node_editor::registry::descriptor_for;

/// Result of evaluating a proposed edge.
#[derive(Debug, PartialEq)]
pub enum EdgeVerdict {
    Ok,
    SelfLoop,
    UnknownSource,
    UnknownTarget,
    BadSocket,
    WouldCycle,
}

/// Can `edge` be added to `graph`? Pure — does not mutate.
pub fn validate_connection(graph: &NodeGraph, edge: &EdgeEntry) -> EdgeVerdict {
    if edge.source == edge.target {
        return EdgeVerdict::SelfLoop;
    }
    let Some(source_entry) = graph.nodes.get(&edge.source) else {
        return EdgeVerdict::UnknownSource;
    };
    let Some(target_entry) = graph.nodes.get(&edge.target) else {
        return EdgeVerdict::UnknownTarget;
    };
    let source_kind = descriptor_for(&source_entry.spec).output;
    let target_arity = descriptor_for(&target_entry.spec).inputs;
    let Some(target_socket_kind) = target_arity.socket_at(edge.target_socket) else {
        return EdgeVerdict::BadSocket;
    };
    if !source_kind.compatible_with(target_socket_kind) {
        return EdgeVerdict::BadSocket;
    }
    if creates_cycle(graph, edge) {
        return EdgeVerdict::WouldCycle;
    }
    EdgeVerdict::Ok
}

/// Would adding `edge` create a cycle? BFS from `edge.target` over outgoing
/// edges; if we reach `edge.source`, it's a cycle.
fn creates_cycle(graph: &NodeGraph, edge: &EdgeEntry) -> bool {
    let mut seen: HashSet<&FlowId> = HashSet::new();
    let mut queue: VecDeque<&FlowId> = VecDeque::new();
    queue.push_back(&edge.target);
    seen.insert(&edge.target);
    while let Some(node) = queue.pop_front() {
        if node == &edge.source {
            return true;
        }
        for e in &graph.edges {
            if &e.source == node && seen.insert(&e.target) {
                queue.push_back(&e.target);
            }
        }
    }
    false
}

/// Color category for an edge, derived from the build state of its source
/// and target. The canvas resolves these to actual `Hsla` via the theme.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EdgeColor {
    /// Same `parent_clock_id` on both ends — paint with the clock's accent.
    /// The `usize` is the clock id mapped into a stable palette index.
    SharedClock(usize),
    /// Crossing clocks but build succeeded (e.g. via a resampler).
    Neutral,
    /// Build failed somewhere in the chain — typically `ClockMismatch`.
    Error,
    /// Either end isn't built yet.
    Pending,
}

pub fn edge_color(graph: &NodeGraph, edge: &EdgeEntry) -> EdgeColor {
    let Some(source) = graph.nodes.get(&edge.source) else {
        return EdgeColor::Pending;
    };
    let Some(target) = graph.nodes.get(&edge.target) else {
        return EdgeColor::Pending;
    };
    if matches!(target.build, BuildState::Error(BuildError::ClockMismatch)) {
        return EdgeColor::Error;
    }
    let (BuildState::Built(s_arc), BuildState::Built(t_arc)) = (&source.build, &target.build)
    else {
        return EdgeColor::Pending;
    };
    match (s_arc.parent_clock_id(), t_arc.parent_clock_id()) {
        (Some(a), Some(b)) if a == b => {
            // Map the u64 clock id to a small palette index. Stable per-clock;
            // the consumer mods by `theme.line_colors.len()`.
            EdgeColor::SharedClock(a.0 as usize)
        }
        _ => EdgeColor::Neutral,
    }
}
