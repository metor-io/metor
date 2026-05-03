//! Aggregates per-editor `alive` sets and reconciles their union into the
//! `DynamicRegistry`. Without this, two editors calling `reconcile`
//! independently would each drop the other's nodes.
//!
//! Nodes are content-addressed by [`NodeId`], so `DynamicRegistry::get_or_build`
//! already dedupes builds across editors. The coordinator only handles the
//! *alive set* aggregation.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{App, Global};

use crate::dynamic::{BuildError, DynamicNode, DynamicRegistry, NodeId};

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
        let union: HashSet<NodeId> = {
            let coord = cx.global_mut::<GraphCoordinator>();
            coord.owners.insert(owner, alive);
            coord.union_alive()
        };
        cx.global_mut::<DynamicRegistry>().reconcile(&union);
    }

    /// Forget an owner entirely — call from the editor's `Drop`.
    pub fn drop_owner(owner: OwnerId, cx: &mut App) {
        let union: HashSet<NodeId> = {
            let coord = cx.global_mut::<GraphCoordinator>();
            if coord.owners.remove(&owner).is_none() {
                return;
            }
            coord.union_alive()
        };
        cx.global_mut::<DynamicRegistry>().reconcile(&union);
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

/// Build helper that goes straight through `DynamicRegistry`. Idempotent: if
/// `id` already exists (built by us or another editor) we share the `Arc`.
pub fn get_or_build_node(
    id: NodeId,
    cx: &mut App,
    build: impl FnOnce() -> Result<Arc<dyn DynamicNode>, BuildError>,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let registry = cx.global_mut::<DynamicRegistry>();
    if let Some(existing) = registry.get(id) {
        return Ok(existing);
    }
    let node = build()?;
    debug_assert_eq!(
        node.id(),
        id,
        "spec::build produced a NodeId that doesn't match compute_node_id"
    );
    registry.insert(node.clone());
    Ok(node)
}
