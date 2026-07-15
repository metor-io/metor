//! Aggregates per-editor `alive` sets and reconciles their union into the
//! `DynamicRegistry`. Without this, two editors calling `reconcile`
//! independently would each drop the other's nodes.
//!
//! Nodes are content-addressed by [`NodeId`], and rebuilds consult
//! `DynamicRegistry::get` before constructing, so builds already dedupe
//! across editors. The coordinator only handles the *alive set* aggregation.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{App, Global};

use crate::dynamic::{DynamicNode, DynamicRegistry, NodeId};
use crate::node_editor::worker::DynamicWorker;

pub type OwnerId = u64;

#[derive(Default)]
pub struct GraphCoordinator {
    owners: HashMap<OwnerId, HashSet<NodeId>>,
}

impl Global for GraphCoordinator {}

impl GraphCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn init(cx: &mut App) {
        cx.set_global(Self::new());
    }

    /// Replace `owner`'s alive set and reconcile the union into the registry.
    pub fn submit(owner: OwnerId, alive: HashSet<NodeId>, cx: &mut App) {
        let union = cx.global_mut::<GraphCoordinator>().set_alive(owner, alive);
        let removed = cx.global_mut::<DynamicRegistry>().reconcile(&union);
        dispose(removed, cx);
    }

    /// Forget an owner entirely and reconcile whatever remains. The editor
    /// pane registers this via `cx.on_release`, so closing a pane drops its
    /// share of the alive set (and any nodes only it kept alive).
    pub fn drop_owner(owner: OwnerId, cx: &mut App) {
        let Some(union) = cx.global_mut::<GraphCoordinator>().forget(owner) else {
            return;
        };
        let removed = cx.global_mut::<DynamicRegistry>().reconcile(&union);
        dispose(removed, cx);
    }

    fn set_alive(&mut self, owner: OwnerId, alive: HashSet<NodeId>) -> HashSet<NodeId> {
        self.owners.insert(owner, alive);
        self.union_alive()
    }

    /// Returns the post-removal union, or `None` if `owner` was unknown.
    fn forget(&mut self, owner: OwnerId) -> Option<HashSet<NodeId>> {
        self.owners.remove(&owner)?;
        Some(self.union_alive())
    }

    fn union_alive(&self) -> HashSet<NodeId> {
        self.owners
            .values()
            .flat_map(|set| set.iter().copied())
            .collect()
    }

    pub fn alive_count(&self, owner: OwnerId) -> usize {
        self.owners.get(&owner).map(|s| s.len()).unwrap_or(0)
    }

    pub fn owner_count(&self) -> usize {
        self.owners.len()
    }
}

/// Hand `removed` to the worker thread so the underlying tasks are
/// cancelled and dropped on the thread that owns the stellarator timer,
/// avoiding the cross-thread spinlock deadlock observed when a `Sleep`-
/// based task (e.g. `fixed_rate`) is destructed off-runtime.
fn dispose(removed: Vec<Arc<dyn DynamicNode>>, cx: &App) {
    if removed.is_empty() {
        return;
    }
    let handle = cx.global::<DynamicWorker>().handle().clone();
    for arc in removed {
        handle.dispose(arc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(raw: &[u64]) -> HashSet<NodeId> {
        raw.iter().copied().map(NodeId).collect()
    }

    #[test]
    fn union_spans_all_owners() {
        let mut coord = GraphCoordinator::new();
        assert_eq!(coord.set_alive(1, ids(&[10, 11])), ids(&[10, 11]));
        assert_eq!(coord.set_alive(2, ids(&[11, 12])), ids(&[10, 11, 12]));
        assert_eq!(coord.owner_count(), 2);
        assert_eq!(coord.alive_count(1), 2);
    }

    #[test]
    fn resubmit_replaces_rather_than_accumulates() {
        let mut coord = GraphCoordinator::new();
        coord.set_alive(1, ids(&[10, 11]));
        assert_eq!(coord.set_alive(1, ids(&[12])), ids(&[12]));
        assert_eq!(coord.alive_count(1), 1);
    }

    #[test]
    fn forget_keeps_nodes_shared_with_surviving_owners() {
        let mut coord = GraphCoordinator::new();
        coord.set_alive(1, ids(&[10, 11]));
        coord.set_alive(2, ids(&[11, 12]));
        assert_eq!(coord.forget(1), Some(ids(&[11, 12])));
        assert_eq!(coord.owner_count(), 1);
        assert_eq!(coord.forget(2), Some(ids(&[])));
        assert_eq!(coord.owner_count(), 0);
    }

    #[test]
    fn forget_unknown_owner_is_a_no_op() {
        let mut coord = GraphCoordinator::new();
        coord.set_alive(1, ids(&[10]));
        assert_eq!(coord.forget(99), None);
        assert_eq!(coord.owner_count(), 1);
    }
}
