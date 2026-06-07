//! Mechanics shared by the GPU-backed plot entities (`time_series::LinePlot`,
//! `xy_plot::XyLinePlot`, `list_plot::ListLinePlot`).
//!
//! The three keep their own struct shapes — the inspector reflects a
//! different set of fields for each (multi-axis vs. flat scalar overrides) —
//! but the per-trace tracker bookkeeping is byte-identical, so it lives here.

use std::collections::{HashMap, HashSet};

use gpui::{Entity, EntityId, Task};

/// Sync the tracker and task maps to the current `traces`: drop entries whose
/// trace disappeared, then `make` a tracker for each newly-added trace.
///
/// Keyed by [`EntityId`] so reordering `traces` leaves live tasks untouched.
pub fn reconcile_trackers<Tr: 'static, K>(
    traces: &[Entity<Tr>],
    tracking: &mut HashMap<EntityId, K>,
    tasks: &mut HashMap<EntityId, Task<()>>,
    mut make: impl FnMut(EntityId, &Entity<Tr>) -> (K, Task<()>),
) {
    let current: HashSet<EntityId> = traces.iter().map(|e| e.entity_id()).collect();
    tracking.retain(|id, _| current.contains(id));
    tasks.retain(|id, _| current.contains(id));
    for trace in traces {
        let id = trace.entity_id();
        if let std::collections::hash_map::Entry::Vacant(slot) = tracking.entry(id) {
            let (state, task) = make(id, trace);
            slot.insert(state);
            tasks.insert(id, task);
        }
    }
}
